// `CapabilityRequest`'s accessors and some `CapabilityKind` helpers are
// exercised only by manifest validation + tests today; they're part of
// the declaration contract, so allow until a consumer wires them.
#![allow(dead_code)]

//! Extension capability surface.
//!
//! Capabilities are the typed contract by which extensions consume host
//! services and each other's services. Input and output types are plain
//! data, so the same trait shapes are reusable across in-process
//! extensions (today) and sandboxed extensions (WASM / subprocess, future).
//!
//! # Two flavors
//!
//! Capabilities split into two ownership groups:
//!
//! * **Host-only** — published by the chaz host; extensions cannot
//!   provide their own impl. Today the only live host-only cap is
//!   [`AgentStateAdmin`] (scoped agent-DB access for the schedule
//!   tools). The remaining host-only kinds in [`CapabilityKind`]
//!   (session access, tool/command registration) are *declaration
//!   vocabulary* only — extensions publish tools/commands and reach
//!   their session structurally through the instance model
//!   (`ExtensionInstance` endpoints + `ScopeCtx`), not through a cap.
//! * **Extension-providable** — [`Messenger`], [`MemoryAccess`],
//!   [`PromptAugmentation`], [`ContextTail`]. An extension publishes an
//!   impl from the matching [`crate::extension::instance::ExtensionInstance`]
//!   endpoint; consumers resolve them at turn time through the
//!   [`crate::extension::instance::CapResolver`].
//!
//! [`CapabilityKind::is_host_only`] is the authoritative split, used by
//! manifest validation to reject host-only kinds in
//! `provides_capabilities`.
//!
//! # No `async_trait`
//!
//! Trait methods return [`CapFuture`] (a manually pinned boxed future),
//! matching the [`crate::extension::hooks`] convention. This keeps the
//! surface object-safe without a proc-macro dependency.

use crate::agent_db::AgentDb;
use crate::hosted_index::DbEntry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

/// Boxed future returned by every cap trait method.
///
/// The `'a` borrow lets implementations hold `&self` across the await
/// without forcing the trait to be `'static`. Mirrors the
/// [`crate::tool::Tool`] / [`crate::extension::hooks`] shape so chaz's
/// trait conventions stay uniform.
pub type CapFuture<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;

// =========================================================================
// CapabilityKind
// =========================================================================

/// The set of capability kinds the host knows about.
///
/// Each extension's manifest declares which kinds it requires, requests,
/// and provides — see [`CapabilityRequest`] for the consumer side and
/// the manifest types for the provider side.
///
/// The split between **host-only** and **extension-providable** is
/// captured by [`Self::is_host_only`]. Host-only kinds cannot appear in
/// `provides_capabilities`; manifest validation rejects them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    /// Read-only access to the calling session's entry log + metadata.
    SessionRead,
    /// Append new entries to the calling session.
    SessionWrite,
    /// Per-session, per-extension settings storage (JSON values).
    Settings,
    /// Register tools that flow into the agent's tool list.
    ToolRegistration,
    /// Register slash commands the gateway can dispatch.
    CommandRegistration,
    /// Send a message to a named target (channel, room, agent...).
    Messenger,
    /// Search / write memory in a named scope.
    Memory,
    /// Read/write agent-owned state (schedules, memory, configuration).
    /// Host-only — the hub scopes each impl to the operator-configured
    /// set of agents before the extension sees it.
    AgentStateAdmin,
    /// Append text to the agent's system prompt at context assembly time.
    /// Extension-providable — extensions like `skills` publish impls;
    /// the host collects and concatenates results.
    PromptAugmentation,
    /// Append text after the conversation messages at context assembly time.
    /// Extension-providable — extensions like 'memory' publish impls;
    /// the host collects and concatenates results at the end of context.
    ContextTail,
}

impl CapabilityKind {
    /// `true` for kinds that only chaz core may publish. These will be
    /// rejected if they appear in an extension's `provides_capabilities`.
    pub fn is_host_only(self) -> bool {
        matches!(
            self,
            Self::SessionRead
                | Self::SessionWrite
                | Self::Settings
                | Self::ToolRegistration
                | Self::CommandRegistration
                | Self::AgentStateAdmin,
        )
    }

    /// `true` for kinds extensions may publish from an instance endpoint.
    /// Exactly the inverse of [`Self::is_host_only`].
    pub fn is_extension_providable(self) -> bool {
        !self.is_host_only()
    }

    /// Lower-snake-case identifier matching the `serde` rename. Used in
    /// log/error messages so the wire and human names stay aligned.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionRead => "session_read",
            Self::SessionWrite => "session_write",
            Self::Settings => "settings",
            Self::ToolRegistration => "tool_registration",
            Self::CommandRegistration => "command_registration",
            Self::Messenger => "messenger",
            Self::Memory => "memory",
            Self::AgentStateAdmin => "agent_state_admin",
            Self::PromptAugmentation => "prompt_augmentation",
            Self::ContextTail => "context_tail",
        }
    }
}

impl fmt::Display for CapabilityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// =========================================================================
// CapabilityRequest
// =========================================================================

/// A consumer-side request for a capability.
///
/// Host-only kinds have no provider choice (the host provides the only
/// impl). Extension-providable kinds carry an optional `provider` name:
/// `None` resolves to the operator's configured default for that kind;
/// `Some(name)` binds to the specific provider that registered under
/// that name.
///
/// Stored in manifests as either `required_capabilities` (load fails if
/// absent) or `requested_capabilities` (degrades gracefully if absent).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilityRequest {
    SessionRead,
    SessionWrite,
    Settings,
    ToolRegistration,
    CommandRegistration,
    Messenger {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    Memory {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Access hosted agent DBs for state operations. The `agents`
    /// field is set by the operator (not the extension) — it carries
    /// the per-extension agent allowlist from `tool_policy`.
    AgentStateAdmin {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agents: Option<Vec<String>>,
    },
    PromptAugmentation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    ContextTail {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
}

impl CapabilityRequest {
    /// Which [`CapabilityKind`] this request resolves against. Useful for
    /// indexing without matching every variant.
    pub fn kind(&self) -> CapabilityKind {
        match self {
            Self::SessionRead => CapabilityKind::SessionRead,
            Self::SessionWrite => CapabilityKind::SessionWrite,
            Self::Settings => CapabilityKind::Settings,
            Self::ToolRegistration => CapabilityKind::ToolRegistration,
            Self::CommandRegistration => CapabilityKind::CommandRegistration,
            Self::Messenger { .. } => CapabilityKind::Messenger,
            Self::Memory { .. } => CapabilityKind::Memory,
            Self::AgentStateAdmin { .. } => CapabilityKind::AgentStateAdmin,
            Self::PromptAugmentation { .. } => CapabilityKind::PromptAugmentation,
            Self::ContextTail { .. } => CapabilityKind::ContextTail,
        }
    }

    /// Provider name carried by the request, if any. `None` means "bind
    /// to the operator default for this kind" for extension-providable
    /// kinds, or "not applicable" for host-only kinds.
    pub fn provider(&self) -> Option<&str> {
        match self {
            Self::Messenger { provider }
            | Self::Memory { provider }
            | Self::PromptAugmentation { provider }
            | Self::ContextTail { provider } => provider.as_deref(),
            _ => None,
        }
    }

    /// Agent allowlist carried by this request, if this is an
    /// `AgentStateAdmin` variant. `None` means "no restriction"
    /// (all hosted agents). `Some(empty)` means "deny-all."
    pub fn agents(&self) -> Option<&[String]> {
        match self {
            Self::AgentStateAdmin { agents } => agents.as_deref(),
            _ => None,
        }
    }
}

// =========================================================================
// Supporting data types
// =========================================================================

/// Payload handed to [`Messenger::send`]. Text is the lowest common
/// denominator; richer attachments are an open-ended JSON list so
/// individual messengers can carry channel-specific extras (e.g. Matrix
/// formatted body, embeds) without locking the wire type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Value>,
}

/// Scope label for memory operations on the [`MemoryAccess`] cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum MemoryScope {
    /// The calling agent's own memory bank.
    Agent,
    /// A named shared bank the agent has been granted access to.
    Bank { name: String },
}

/// One hit returned by [`MemoryAccess::search`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHit {
    pub key: String,
    pub value: String,
    /// Provider-defined relevance score; higher is more relevant.
    /// Comparable only within a single search result set.
    pub score: f32,
    /// Bank the hit came from. `None` when the search ran against the
    /// agent's own memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bank: Option<String>,
}

// =========================================================================
// Host-only cap traits
// =========================================================================

/// Narrow capability: access hosted agent DBs for state operations
/// (schedules, memory, configuration). The hub scopes each impl to the
/// operator-configured set of agents before the extension sees it.
///
/// This is a **guardrail, not a sandbox** — it stops a poorly behaved
/// tool from accidentally touching the wrong agent's DB, but does not
/// attempt to prevent an adversarial tool from escalating privileges
/// through other means.
pub trait AgentStateAdmin: Send + Sync {
    /// Resolve an agent name or DB id to its `DbEntry`. Only agents in
    /// the operator-configured allowlist are visible; disallowed names
    /// return `Err(...)`.
    fn resolve_agent(&self, name: &str) -> Result<DbEntry, String>;

    /// Open the agent DB identified by `entry`. Must be a `DbEntry`
    /// obtained from `resolve_agent` on the same handle. The impl uses
    /// the peer's held key to open the DB.
    fn open_agent_db<'a>(&'a self, entry: &'a DbEntry) -> CapFuture<'a, AgentDb>;
}

// =========================================================================
// Extension-providable cap traits
// =========================================================================

/// Send a message to a provider-defined target.
///
/// `target` is a string the provider interprets — a Matrix room id, an
/// email address, an agent name. No typed `MessengerTarget` enum: each
/// messenger uses its own addressing scheme and consumers pick a
/// provider by name when they need a specific scheme.
pub trait Messenger: Send + Sync {
    fn send<'a>(&'a self, target: String, body: MessageBody) -> CapFuture<'a, ()>;
}

/// Search and write memory in a named scope.
///
/// The contract intentionally mirrors the existing `remember` / `recall`
/// built-in tools so the in-tree `memory` extension can register as
/// the canonical provider with thin glue. The `agent_name` argument
/// identifies the *calling* agent — required for [`MemoryScope::Agent`]
/// to resolve the right per-agent store, and recorded as the writer for
/// bank scope.
pub trait MemoryAccess: Send + Sync {
    fn search<'a>(
        &'a self,
        agent_name: &'a str,
        query: &'a str,
        scope: MemoryScope,
    ) -> CapFuture<'a, Vec<MemoryHit>>;
    fn remember<'a>(
        &'a self,
        agent_name: &'a str,
        key: &'a str,
        value: &'a str,
        scope: MemoryScope,
    ) -> CapFuture<'a, ()>;
}

/// Append text to the agent's system prompt during context assembly.
///
/// Extension-providable — extensions like `skills` publish an impl;
/// the host calls every provider and concatenates non-empty results
/// after the agent's core system prompt. The augmentation receives
/// the agent and the session's recent message text so it can decide
/// whether to contribute.
/// The method is async so providers that need database access (e.g.
/// memory surfacing) can use eidetica without blocking. Extensions
/// that hold their data purely in memory (skills, rules) return
/// immediately.
pub trait PromptAugmentation: Send + Sync {
    /// Return additional system prompt text, or `None` if this
    /// extension has nothing to contribute for this turn.
    fn augment_system_prompt<'a>(
        &'a self,
        agent_name: &'a str,
        recent_message_text: &'a [String],
    ) -> CapFuture<'a, Option<String>>;
}

/// Append text after the conversation messages at context assembly time.
///
/// Extension-providable — extensions like 'memory' publish an impl;
/// the host calls every provider and concatenates non-empty results
/// after the assembled messages. Unlike [`PromptAugmentation`], this
/// fires at the end of context, not in the system prompt.
pub trait ContextTail: Send + Sync {
    /// Return additional text to append after the conversation messages,
    /// or `None` if this extension has nothing to contribute for this turn.
    fn context_tail<'a>(
        &'a self,
        agent_name: &'a str,
        recent_message_text: &'a [String],
    ) -> CapFuture<'a, Option<String>>;
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_host_only_partitions_kinds_exhaustively() {
        let host_only = [
            CapabilityKind::SessionRead,
            CapabilityKind::SessionWrite,
            CapabilityKind::Settings,
            CapabilityKind::ToolRegistration,
            CapabilityKind::CommandRegistration,
            CapabilityKind::AgentStateAdmin,
        ];
        for k in host_only {
            assert!(k.is_host_only(), "{k} should be host-only");
            assert!(!k.is_extension_providable(), "{k} should not be providable");
        }
        let providable = [CapabilityKind::Messenger, CapabilityKind::Memory];
        for k in providable {
            assert!(!k.is_host_only(), "{k} should not be host-only");
            assert!(k.is_extension_providable(), "{k} should be providable");
        }
    }

    #[test]
    fn capability_kind_serde_uses_snake_case() {
        let cases = [
            (CapabilityKind::SessionRead, "\"session_read\""),
            (CapabilityKind::SessionWrite, "\"session_write\""),
            (CapabilityKind::Settings, "\"settings\""),
            (CapabilityKind::ToolRegistration, "\"tool_registration\""),
            (
                CapabilityKind::CommandRegistration,
                "\"command_registration\"",
            ),
            (CapabilityKind::Messenger, "\"messenger\""),
            (CapabilityKind::Memory, "\"memory\""),
            (CapabilityKind::AgentStateAdmin, "\"agent_state_admin\""),
        ];
        for (kind, wire) in cases {
            let s = serde_json::to_string(&kind).unwrap();
            assert_eq!(s, wire, "serialize {kind}");
            assert_eq!(kind.as_str(), &wire[1..wire.len() - 1]);
            let round: CapabilityKind = serde_json::from_str(&s).unwrap();
            assert_eq!(round, kind);
        }
    }

    #[test]
    fn capability_request_kind_accessor_covers_every_variant() {
        let cases: Vec<(CapabilityRequest, CapabilityKind)> = vec![
            (CapabilityRequest::SessionRead, CapabilityKind::SessionRead),
            (
                CapabilityRequest::SessionWrite,
                CapabilityKind::SessionWrite,
            ),
            (CapabilityRequest::Settings, CapabilityKind::Settings),
            (
                CapabilityRequest::ToolRegistration,
                CapabilityKind::ToolRegistration,
            ),
            (
                CapabilityRequest::CommandRegistration,
                CapabilityKind::CommandRegistration,
            ),
            (
                CapabilityRequest::Messenger { provider: None },
                CapabilityKind::Messenger,
            ),
            (
                CapabilityRequest::Messenger {
                    provider: Some("matrix".into()),
                },
                CapabilityKind::Messenger,
            ),
            (
                CapabilityRequest::Memory { provider: None },
                CapabilityKind::Memory,
            ),
            (
                CapabilityRequest::AgentStateAdmin { agents: None },
                CapabilityKind::AgentStateAdmin,
            ),
            (
                CapabilityRequest::AgentStateAdmin {
                    agents: Some(vec!["chaz".into()]),
                },
                CapabilityKind::AgentStateAdmin,
            ),
        ];
        for (req, expected) in cases {
            assert_eq!(req.kind(), expected);
        }
    }

    #[test]
    fn capability_request_provider_is_only_set_for_providable_kinds() {
        assert_eq!(CapabilityRequest::SessionRead.provider(), None);
        assert_eq!(CapabilityRequest::Settings.provider(), None);
        assert_eq!(
            CapabilityRequest::AgentStateAdmin { agents: None }.provider(),
            None
        );
        assert_eq!(
            CapabilityRequest::Messenger { provider: None }.provider(),
            None
        );
        assert_eq!(
            CapabilityRequest::Messenger {
                provider: Some("matrix".into())
            }
            .provider(),
            Some("matrix"),
        );
        assert_eq!(
            CapabilityRequest::Memory {
                provider: Some("store".into())
            }
            .provider(),
            Some("store"),
        );
    }

    #[test]
    fn agent_state_admin_request_carries_agents() {
        let req = CapabilityRequest::AgentStateAdmin { agents: None };
        assert_eq!(req.agents(), None);

        let req = CapabilityRequest::AgentStateAdmin {
            agents: Some(vec!["chaz".into(), "bash".into()]),
        };
        assert_eq!(
            req.agents(),
            Some(&["chaz".to_string(), "bash".to_string()][..])
        );

        let req = CapabilityRequest::AgentStateAdmin {
            agents: Some(vec![]),
        };
        assert_eq!(req.agents(), Some(&[][..]));
    }

    #[test]
    fn capability_request_serde_round_trip_with_and_without_provider() {
        // Bare host-only kind serializes as `{"kind":"settings"}`.
        let bare = CapabilityRequest::Settings;
        let s = serde_json::to_string(&bare).unwrap();
        assert_eq!(s, r#"{"kind":"settings"}"#);
        let round: CapabilityRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(round, bare);

        // Providable kind without provider skips the field on the wire.
        let no_provider = CapabilityRequest::Messenger { provider: None };
        let s = serde_json::to_string(&no_provider).unwrap();
        assert_eq!(s, r#"{"kind":"messenger"}"#);
        let round: CapabilityRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(round, no_provider);

        // Providable kind with provider serializes it.
        let with_provider = CapabilityRequest::Memory {
            provider: Some("local".into()),
        };
        let s = serde_json::to_string(&with_provider).unwrap();
        assert_eq!(s, r#"{"kind":"memory","provider":"local"}"#);
        let round: CapabilityRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(round, with_provider);

        // AgentStateAdmin without agents skips the field on the wire.
        let no_agents = CapabilityRequest::AgentStateAdmin { agents: None };
        let s = serde_json::to_string(&no_agents).unwrap();
        assert_eq!(s, r#"{"kind":"agent_state_admin"}"#);
        let round: CapabilityRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(round, no_agents);

        // AgentStateAdmin with agents serializes them.
        let with_agents = CapabilityRequest::AgentStateAdmin {
            agents: Some(vec!["chaz".into()]),
        };
        let s = serde_json::to_string(&with_agents).unwrap();
        assert_eq!(s, r#"{"kind":"agent_state_admin","agents":["chaz"]}"#);
        let round: CapabilityRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(round, with_agents);
    }

    #[test]
    fn memory_scope_serde_round_trip_for_both_variants() {
        let agent = MemoryScope::Agent;
        let s = serde_json::to_string(&agent).unwrap();
        assert_eq!(s, r#"{"scope":"agent"}"#);
        let round: MemoryScope = serde_json::from_str(&s).unwrap();
        assert_eq!(round, agent);

        let bank = MemoryScope::Bank {
            name: "project-alpha".into(),
        };
        let s = serde_json::to_string(&bank).unwrap();
        assert_eq!(s, r#"{"scope":"bank","name":"project-alpha"}"#);
        let round: MemoryScope = serde_json::from_str(&s).unwrap();
        assert_eq!(round, bank);
    }

    #[test]
    fn message_body_skips_empty_attachments() {
        let body = MessageBody {
            text: "hello".into(),
            attachments: Vec::new(),
        };
        let s = serde_json::to_string(&body).unwrap();
        // No `attachments` on the wire when the list is empty.
        assert_eq!(s, r#"{"text":"hello"}"#);
        let round: MessageBody = serde_json::from_str(&s).unwrap();
        assert_eq!(round, body);
    }

    #[test]
    fn memory_hit_serde_handles_missing_bank() {
        // Hit from agent-self search has no bank — must be omitted on
        // the wire and absent on read.
        let hit = MemoryHit {
            key: "deadline".into(),
            value: "Friday".into(),
            score: 0.95,
            bank: None,
        };
        let s = serde_json::to_string(&hit).unwrap();
        assert!(!s.contains("\"bank\""), "got: {s}");
        let round: MemoryHit = serde_json::from_str(&s).unwrap();
        assert_eq!(round.bank, None);
        assert_eq!(round.key, "deadline");
        assert!((round.score - 0.95).abs() < 1e-6);
    }
}
