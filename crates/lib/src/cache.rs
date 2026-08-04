//! Prompt-cache breakpoint policy shared by the LLM backends.
//!
//! Anthropic prompt caching is driven by `cache_control` breakpoints. The
//! *policy* — which structural regions get a breakpoint, in what order, under
//! Anthropic's hard cap of 4 per request — is identical whether the marker
//! rides inline on an OpenAI-compatible request (the OpenRouter→Anthropic path
//! in [`crate::openai`]) or as a first-class field on a native Anthropic
//! request ([`crate::anthropic`]). Only the wire serialization differs, so the
//! policy lives here and each backend stamps its own wire type.

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

/// A region of an assembled request that can carry a prompt-cache breakpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheRegion {
    /// End of the tool-schema block (last tool).
    LastTool,
    /// System prompt — head of the stable prefix.
    System,
    /// Latest user message — the boundary intra-turn tool round-trips share.
    LatestUser,
}

/// Ordered breakpoint plan, most cache-stable region first so the longest-lived
/// prefix is covered even if [`MAX_BREAKPOINTS`] ever forces later slots to
/// drop. Both backends iterate this and stamp their own wire type.
pub const CACHE_PLAN: [CacheRegion; 3] = [
    CacheRegion::LastTool,
    CacheRegion::System,
    CacheRegion::LatestUser,
];
