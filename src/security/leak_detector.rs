use regex::Regex;
use std::sync::LazyLock;

/// Policy for handling detected secret leaks
#[derive(Clone, Debug, Default, PartialEq)]
pub enum LeakPolicy {
    /// Replace detected secrets with [REDACTED:type] placeholders
    #[default]
    Redact,
    /// Reject the entire output if any secret is detected
    Block,
}

/// A pattern that matches a known secret format
struct LeakPattern {
    name: &'static str,
    regex: Regex,
}

/// Scans text for patterns matching known API keys, tokens, and credentials.
/// Applied to all tool outputs before they enter the LLM context.
#[derive(Clone)]
pub struct LeakDetector {
    policy: LeakPolicy,
}

static PATTERNS: LazyLock<Vec<LeakPattern>> = LazyLock::new(|| {
    vec![
        LeakPattern {
            name: "OpenAI API key",
            regex: Regex::new(r"sk-[a-zA-Z0-9]{20,}").unwrap(),
        },
        LeakPattern {
            name: "Anthropic API key",
            regex: Regex::new(r"sk-ant-[a-zA-Z0-9\-]{20,}").unwrap(),
        },
        LeakPattern {
            name: "OpenRouter API key",
            regex: Regex::new(r"sk-or-v1-[a-f0-9]{64}").unwrap(),
        },
        LeakPattern {
            name: "GitHub token",
            regex: Regex::new(r"gh[ps]_[a-zA-Z0-9]{36,}").unwrap(),
        },
        LeakPattern {
            name: "GitHub fine-grained token",
            regex: Regex::new(r"github_pat_[a-zA-Z0-9_]{22,}").unwrap(),
        },
        LeakPattern {
            name: "AWS access key",
            regex: Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
        },
        LeakPattern {
            name: "Stripe key",
            regex: Regex::new(r"[sr]k_(live|test)_[a-zA-Z0-9]{20,}").unwrap(),
        },
        LeakPattern {
            name: "Slack token",
            regex: Regex::new(r"xox[bpoas]-[a-zA-Z0-9\-]{10,}").unwrap(),
        },
        LeakPattern {
            name: "SSH private key",
            regex: Regex::new(r"-----BEGIN OPENSSH PRIVATE KEY-----").unwrap(),
        },
        LeakPattern {
            name: "PEM private key",
            regex: Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").unwrap(),
        },
        LeakPattern {
            name: "Bearer token",
            regex: Regex::new(r"Bearer [a-zA-Z0-9._\-]{20,}").unwrap(),
        },
        LeakPattern {
            name: "SendGrid key",
            regex: Regex::new(r"SG\.[a-zA-Z0-9_\-]{22,}\.[a-zA-Z0-9_\-]{22,}").unwrap(),
        },
    ]
});

impl LeakDetector {
    pub fn new(policy: LeakPolicy) -> Self {
        Self { policy }
    }

    /// Scan text and apply the configured policy.
    /// Returns Ok(possibly_redacted_text) or Err if policy is Block and secrets found.
    pub fn scan(&self, text: &str) -> Result<String, String> {
        let detections = self.detect(text);

        if detections.is_empty() {
            return Ok(text.to_string());
        }

        match self.policy {
            LeakPolicy::Redact => {
                let mut result = text.to_string();
                for (name, pattern) in &detections {
                    result = pattern
                        .replace_all(&result, format!("[REDACTED:{name}]"))
                        .to_string();
                }
                Ok(result)
            }
            LeakPolicy::Block => {
                let names: Vec<&str> = detections.iter().map(|(n, _)| *n).collect();
                Err(format!(
                    "Tool output blocked: detected potential secrets ({})",
                    names.join(", ")
                ))
            }
        }
    }

    /// Returns list of (pattern_name, regex) for all patterns that matched.
    fn detect<'a>(&self, text: &str) -> Vec<(&'a str, &'a Regex)> {
        PATTERNS
            .iter()
            .filter(|p| p.regex.is_match(text))
            .map(|p| (p.name, &p.regex))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redacts_openai_key() {
        let detector = LeakDetector::new(LeakPolicy::Redact);
        let input = "Found key: sk-abc123def456ghi789jkl012mno";
        let result = detector.scan(input).unwrap();
        assert!(result.contains("[REDACTED:OpenAI API key]"));
        assert!(!result.contains("sk-abc123"));
    }

    #[test]
    fn test_redacts_openrouter_key() {
        let detector = LeakDetector::new(LeakPolicy::Redact);
        let key = format!("api_key: \"sk-or-v1-{}\"", "a".repeat(64));
        let result = detector.scan(&key).unwrap();
        assert!(result.contains("[REDACTED:OpenRouter API key]"));
    }

    #[test]
    fn test_redacts_github_token() {
        let detector = LeakDetector::new(LeakPolicy::Redact);
        let input = format!("token: ghp_{}", "a".repeat(36));
        let result = detector.scan(&input).unwrap();
        assert!(result.contains("[REDACTED:GitHub token]"));
    }

    #[test]
    fn test_redacts_pem_key() {
        let detector = LeakDetector::new(LeakPolicy::Redact);
        let input = "-----BEGIN RSA PRIVATE KEY-----\nMIIE...";
        let result = detector.scan(input).unwrap();
        assert!(result.contains("[REDACTED:PEM private key]"));
    }

    #[test]
    fn test_redacts_ssh_key() {
        let detector = LeakDetector::new(LeakPolicy::Redact);
        let input = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blb...";
        let result = detector.scan(input).unwrap();
        assert!(result.contains("[REDACTED:SSH private key]"));
    }

    #[test]
    fn test_block_policy_rejects() {
        let detector = LeakDetector::new(LeakPolicy::Block);
        let input = format!("key: sk-or-v1-{}", "a".repeat(64));
        let result = detector.scan(&input);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("blocked"));
    }

    #[test]
    fn test_clean_text_passes() {
        let detector = LeakDetector::new(LeakPolicy::Redact);
        let input = "This is normal text with no secrets.";
        let result = detector.scan(input).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn test_multiple_secrets_redacted() {
        let detector = LeakDetector::new(LeakPolicy::Redact);
        let input = format!(
            "openai: sk-abc123def456ghi789jkl012mno\ngithub: ghp_{}",
            "b".repeat(36)
        );
        let result = detector.scan(&input).unwrap();
        assert!(result.contains("[REDACTED:OpenAI API key]"));
        assert!(result.contains("[REDACTED:GitHub token]"));
    }

    #[test]
    fn test_aws_key_detected() {
        let detector = LeakDetector::new(LeakPolicy::Redact);
        let input = "aws_access_key_id = AKIAIOSFODNN7EXAMPLE";
        let result = detector.scan(input).unwrap();
        assert!(result.contains("[REDACTED:AWS access key]"));
    }
}
