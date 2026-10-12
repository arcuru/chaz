//! Provider-neutral cache options and adapter wire markers.
//!
//! Extensions choose anchors/TTL; adapters place supported markers and omit
//! them on incompatible providers. The built-in strategy owns the defaults.

use serde::{Deserialize, Serialize};

/// Anthropic rejects requests carrying more than this many `cache_control`
/// breakpoints.
pub const MAX_BREAKPOINTS: u8 = 4;

/// Anthropic prompt-cache breakpoint marker. `ttl` omitted → default 5-minute
/// cache. On the OpenAI-compatible path this rides inside a content part (or on
/// a tool object) and OpenRouter forwards it to Anthropic; on the native path
/// it is a first-class field on system/tool/content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        CacheControl {
            kind: "ephemeral".to_string(),
            ttl: None,
        }
    }
}

/// Provider-neutral supported anchors. Message indices refer to the final
/// projected request, before adapter coalescing. They cannot carry raw fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheAnchor {
    LastTool,
    System,
    LatestUser,
    Message(usize),
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    #[default]
    FiveMinutes,
    OneHour,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheOptions {
    pub anchors: Vec<CacheAnchor>,
    #[serde(default)]
    pub ttl: CacheTtl,
}
impl CacheOptions {
    pub fn validate(&self, messages: &[crate::runtime::RuntimeMessage]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.anchors.len() <= MAX_BREAKPOINTS as usize,
            "too many cache anchors"
        );
        let mut seen = std::collections::HashSet::new();
        for anchor in &self.anchors {
            anyhow::ensure!(seen.insert(*anchor), "duplicate cache anchor");
            if let CacheAnchor::Message(i) = anchor {
                anyhow::ensure!(
                    matches!(
                        messages.get(*i),
                        Some(
                            crate::runtime::RuntimeMessage::User(_)
                                | crate::runtime::RuntimeMessage::Assistant(_)
                                | crate::runtime::RuntimeMessage::ToolResult { .. }
                        )
                    ),
                    "unsupported cache message anchor"
                );
            }
        }
        Ok(())
    }
    pub(crate) fn control(&self) -> CacheControl {
        CacheControl {
            kind: "ephemeral".into(),
            ttl: match self.ttl {
                CacheTtl::FiveMinutes => None,
                CacheTtl::OneHour => Some("1h".into()),
            },
        }
    }
}
