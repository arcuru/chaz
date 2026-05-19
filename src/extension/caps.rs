// Step 1 of the cap refactor is a pure addition: every public item here
// will be consumed by later steps (manifest/registry/hub). Allow until
// they're wired up rather than littering per-item `#[allow]`s.
#![allow(dead_code)]

//! Extension capability surface.
//!
//! Capabilities are the typed contract by which extensions consume host
//! services and each other's services. They replace the
//! `Arc<Mutex<Session>>` handle today's `HookContext` exposes with narrow,
//! purpose-specific async traits whose input and output types are plain
//! data — making the same trait shapes reusable across in-process
//! extensions (today) and sandboxed extensions (WASM / subprocess, future).
//!
//! # Two flavors
//!
//! Capabilities split into two ownership groups:
//!
//! * **Host-only** — `SessionRead`, `SessionWrite`, `Settings`,
//!   `ToolRegistration`, `CommandRegistration`. Provided by the chaz host;
//!   extensions cannot publish their own impls. Each session has exactly
//!   one impl of each.
//! * **Extension-providable** — `Messenger`, `MemoryAccess`. Any
//!   extension can publish an impl via `build_providers()`; consumers
//!   resolve a provider by kind plus optional provider name. Zero, one,
//!   or many providers per kind may register.
//!
//! [`CapabilityKind::is_host_only`] is the authoritative split. Putting a
//! host-only kind in `provides_capabilities` is a manifest-validation
//! error (enforced in step 2 of the refactor).
//!
//! # No `async_trait`
//!
//! Trait methods return `CapFuture<'a, T>` (a manually pinned boxed
//! future), matching the [`crate::extension::hooks`] convention. This
//! keeps the surface object-safe without a proc-macro dependency.
//!
//! # Step 1 scope
//!
//! This module is a pure addition: types and trait definitions, no
//! impls and no wiring. Manifests, registries, and the per-session
//! bundle land in subsequent refactor steps. See
//! `~/brain/ava/workspace/chaz-routine-engine-and-capabilities.md` for
//! the full plan.

use crate::agent_db::AgentDb;
use crate::extension::ExtensionCommand;
use crate::hosted_index::DbEntry;
use crate::tool::{Tool, ToolDescriptor};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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
/// New kinds are added here when a new cap trait is introduced. Each
/// extension's manifest declares which kinds it requires, requests, and
/// provides — see [`CapabilityRequest`] for the consumer side and the
/// manifest types (added in step 2) for the provider side.
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

    /// `true` for kinds extensions may publish via `build_providers()`.
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
}

impl CapabilityRequest {
    /// Which [`CapabilityKind`] this request resolves against. Useful for
    /// indexing into the cap registry without matching every variant.
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
        }
    }

    /// Provider name carried by the request, if any. `None` means "bind
    /// to the operator default for this kind" for extension-providable
    /// kinds, or "not applicable" for host-only kinds.
    pub fn provider(&self) -> Option<&str> {
        match self {
            Self::Messenger { provider }
            | Self::Memory { provider }
            | Self::PromptAugmentation { provider } => provider.as_deref(),
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
// CapSet
// =========================================================================

/// Per-kind bundle of extension-providable caps a consumer receives.
///
/// Holds at most one impl per name plus an optional `default` slot. A
/// bare request (`Messenger { provider: None }`) resolves to `default`;
/// a named request (`Messenger { provider: Some("matrix") }`) resolves
/// to the corresponding entry in `named`.
///
/// `T: ?Sized` so callers parameterize over the cap trait directly:
/// `CapSet<dyn Messenger>` rather than wrapping in a separate handle.
pub struct CapSet<T: ?Sized> {
    /// Operator-configured default provider for this kind. `None` when
    /// the operator hasn't picked one and no provider auto-defaults
    /// (zero or multiple registered providers).
    pub default: Option<Arc<T>>,
    /// All providers registered for this kind, keyed by their extension
    /// name. The default is *additionally* present here when set.
    pub named: HashMap<String, Arc<T>>,
}

impl<T: ?Sized> CapSet<T> {
    /// Empty `CapSet`: no default, no named providers.
    pub fn new() -> Self {
        Self {
            default: None,
            named: HashMap::new(),
        }
    }

    /// Resolve a consumer request.
    ///
    /// * `None` — return the operator default if one was configured.
    /// * `Some(name)` — return the named provider regardless of what the
    ///   default is. Names that don't exist resolve to `None` even if a
    ///   default is set; the consumer asked for a specific provider and
    ///   the host doesn't silently substitute.
    pub fn get(&self, name: Option<&str>) -> Option<&Arc<T>> {
        match name {
            Some(n) => self.named.get(n),
            None => self.default.as_ref(),
        }
    }

    /// `true` when no providers are registered and no default is set.
    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.named.is_empty()
    }

    /// Names of every registered provider, sorted for deterministic
    /// iteration. Useful for diagnostics / `/extensions list -v`.
    pub fn provider_names(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.named.keys().map(String::as_str).collect();
        out.sort_unstable();
        out
    }
}

impl<T: ?Sized> Default for CapSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Supporting data types
// =========================================================================

/// Opaque cursor returned by [`SessionRead::entries`] paging. Treat the
/// inner string as host-defined; consumers should not parse it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntryCursor(pub String);

/// Identifier returned by [`SessionWrite::append`] and embedded in
/// [`SessionEntryView`]. Opaque — content-addressed by the underlying
/// eidetica entry id today; consumers should treat it as a string token.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntryId(pub String);

/// Read-side view of one session entry. Capture is intentionally
/// minimal — extensions that need richer entry shape can deserialize
/// `data` against their own schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntryView {
    pub id: EntryId,
    /// Entry kind tag (e.g. `"user_message"`, `"directive"`,
    /// `"tool_call"`). Stable across chaz versions.
    pub kind: String,
    pub data: Value,
    pub timestamp: DateTime<Utc>,
}

/// Write-side draft for [`SessionWrite::append`]. The host assigns
/// `id` and `timestamp` on commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntryDraft {
    pub kind: String,
    pub data: Value,
}

/// Stable per-session metadata returned by [`SessionRead::meta`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Descriptor passed to [`CommandRegistration::register`]. Mirrors
/// [`ToolDescriptor`] so the two registration caps share a shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDescriptor {
    pub name: String,
    pub description: String,
}

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

/// Read-only access to the calling session.
pub trait SessionRead: Send + Sync {
    /// Entries on the session, optionally constrained to those after
    /// `since` (exclusive). Cursor semantics are host-defined; an opaque
    /// cursor returned from a prior call paginates forward in time.
    fn entries<'a>(&'a self, since: Option<EntryCursor>) -> CapFuture<'a, Vec<SessionEntryView>>;

    /// Stable per-session metadata.
    fn meta<'a>(&'a self) -> CapFuture<'a, SessionMeta>;
}

/// Append entries to the calling session.
pub trait SessionWrite: Send + Sync {
    /// Append one entry. The host assigns the [`EntryId`] and timestamp
    /// on commit and returns the id for callers that need to reference
    /// the row later.
    fn append<'a>(&'a self, entry: SessionEntryDraft) -> CapFuture<'a, EntryId>;
}

/// Per-extension, per-session settings storage.
///
/// Values are opaque JSON — extensions deserialize against their own
/// typed shapes. Missing keys read as `Ok(None)`; that's the canonical
/// "no override" signal.
pub trait Settings: Send + Sync {
    fn get<'a>(&'a self, key: &'a str) -> CapFuture<'a, Option<Value>>;
    fn set<'a>(&'a self, key: &'a str, value: Value) -> CapFuture<'a, ()>;
}

/// Register tools the agent can call.
///
/// Called from `Extension::install`; the registry seals after install
/// completes. Re-registering a name later is the host's choice (today:
/// first-write-wins, like the existing hub).
pub trait ToolRegistration: Send + Sync {
    fn register<'a>(&'a self, descriptor: ToolDescriptor, tool: Arc<dyn Tool>)
    -> CapFuture<'a, ()>;
}

/// Register slash commands the gateway can dispatch.
pub trait CommandRegistration: Send + Sync {
    fn register<'a>(
        &'a self,
        descriptor: CommandDescriptor,
        command: Box<dyn ExtensionCommand>,
    ) -> CapFuture<'a, ()>;
}

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
/// the canonical provider with thin glue.
pub trait MemoryAccess: Send + Sync {
    fn search<'a>(&'a self, query: &'a str, scope: MemoryScope) -> CapFuture<'a, Vec<MemoryHit>>;
    fn remember<'a>(
        &'a self,
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
///
/// This is intentionally synchronous — extensions hold their data in
/// memory (skill registry, surfacing index) and string building is
/// cheap. Async is unnecessary for the `Option<String>` return shape.
pub trait PromptAugmentation: Send + Sync {
    /// Return additional system prompt text, or `None` if this
    /// extension has nothing to contribute for this turn.
    fn augment_system_prompt(
        &self,
        agent_name: &str,
        recent_message_text: &[String],
    ) -> Option<String>;
}

// =========================================================================
// CapProvider
// =========================================================================

/// One impl an extension publishes via `build_providers()`.
///
/// One variant per extension-providable kind. The host registers
/// providers into the cap registry keyed by `(kind, extension_name)`,
/// then resolves consumer requests against that registry when building
/// each consumer's bundle.
pub enum CapProvider {
    Messenger(Arc<dyn Messenger>),
    Memory(Arc<dyn MemoryAccess>),
    AgentStateAdmin(Arc<dyn AgentStateAdmin>),
    PromptAugmentation(Arc<dyn PromptAugmentation>),
}

impl CapProvider {
    /// Which [`CapabilityKind`] this provider satisfies.
    pub fn kind(&self) -> CapabilityKind {
        match self {
            Self::Messenger(_) => CapabilityKind::Messenger,
            Self::Memory(_) => CapabilityKind::Memory,
            Self::PromptAugmentation(_) => CapabilityKind::PromptAugmentation,
            Self::AgentStateAdmin(_) => CapabilityKind::AgentStateAdmin,
        }
    }
}

impl fmt::Debug for CapProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Skip the `Arc` payload — `dyn Trait` isn't `Debug` and the
        // kind is the only useful thing to print.
        f.debug_tuple("CapProvider").field(&self.kind()).finish()
    }
}

// =========================================================================
// ExtensionCaps — per-extension consumer bundle
// =========================================================================

/// The fully-resolved bundle of capabilities a consumer extension
/// receives at `install` time.
///
/// One field per host-only kind (each holds at most one impl — the
/// host's), plus a [`CapSet`] per extension-providable kind so the
/// consumer can either grab the operator default (bare request) or
/// look up a specific provider by name.
///
/// Slots are `Option` / empty `CapSet` whenever the extension's
/// manifest didn't grant the corresponding cap. Attempting to use a
/// missing slot is a logic error on the extension's part — the manifest
/// contract said it wouldn't.
///
/// Built by the hub during phase 2 of `install_all` (added in step 5)
/// from the operator config, the cap registry, and the requesting
/// extension's manifest.
pub struct ExtensionCaps {
    pub session_read: Option<Arc<dyn SessionRead>>,
    pub session_write: Option<Arc<dyn SessionWrite>>,
    pub settings: Option<Arc<dyn Settings>>,
    pub tool_registration: Option<Arc<dyn ToolRegistration>>,
    pub command_registration: Option<Arc<dyn CommandRegistration>>,
    /// Scoped agent state access — pre-built by the hub from the
    /// operator's `tool_policy` agent allowlist. `None` when the
    /// extension didn't request this cap or the allowlist intersected
    /// to empty (cap denied).
    pub agent_state_admin: Option<Arc<dyn AgentStateAdmin>>,
    pub messengers: CapSet<dyn Messenger>,
    pub memory: CapSet<dyn MemoryAccess>,
    pub prompt_augmentation: CapSet<dyn PromptAugmentation>,
}

impl ExtensionCaps {
    /// Bundle with no caps granted. Convenient for tests and for
    /// extensions whose manifests grant nothing (e.g., pure hook
    /// observers that don't consume any cap).
    pub fn empty() -> Self {
        Self {
            session_read: None,
            session_write: None,
            settings: None,
            tool_registration: None,
            command_registration: None,
            agent_state_admin: None,
            messengers: CapSet::new(),
            memory: CapSet::new(),
            prompt_augmentation: CapSet::new(),
        }
    }

    /// `true` when no host-only slot is filled and both `CapSet`s are
    /// empty. Useful in tests and as a manifest sanity check.
    pub fn is_empty(&self) -> bool {
        self.session_read.is_none()
            && self.session_write.is_none()
            && self.settings.is_none()
            && self.tool_registration.is_none()
            && self.command_registration.is_none()
            && self.agent_state_admin.is_none()
            && self.messengers.is_empty()
            && self.memory.is_empty()
            && self.prompt_augmentation.is_empty()
    }
}

impl Default for ExtensionCaps {
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Debug for ExtensionCaps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionCaps")
            .field("session_read", &self.session_read.is_some())
            .field("session_write", &self.session_write.is_some())
            .field("settings", &self.settings.is_some())
            .field("tool_registration", &self.tool_registration.is_some())
            .field("command_registration", &self.command_registration.is_some())
            .field("agent_state_admin", &self.agent_state_admin.is_some())
            .field("messengers", &self.messengers.provider_names())
            .field("memory", &self.memory.provider_names())
            .finish()
    }
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

    // --- CapSet -----------------------------------------------------------

    trait Greeter: Send + Sync {
        fn greeting(&self) -> &str;
    }
    struct Hello(&'static str);
    impl Greeter for Hello {
        fn greeting(&self) -> &str {
            self.0
        }
    }

    #[test]
    fn empty_capset_returns_none_for_default_and_named() {
        let set: CapSet<dyn Greeter> = CapSet::new();
        assert!(set.is_empty());
        assert!(set.get(None).is_none());
        assert!(set.get(Some("anything")).is_none());
        assert!(set.provider_names().is_empty());
    }

    #[test]
    fn capset_default_only_resolves_for_bare_request() {
        let mut set: CapSet<dyn Greeter> = CapSet::new();
        set.default = Some(Arc::new(Hello("hi")));
        assert!(!set.is_empty());
        assert_eq!(set.get(None).unwrap().greeting(), "hi");
        // A named request must not silently fall through to the default.
        assert!(set.get(Some("missing")).is_none());
    }

    #[test]
    fn capset_named_lookup_is_exact_match() {
        let mut set: CapSet<dyn Greeter> = CapSet::new();
        set.named
            .insert("matrix".into(), Arc::new(Hello("from matrix")));
        set.named
            .insert("email".into(), Arc::new(Hello("from email")));
        assert_eq!(set.get(Some("matrix")).unwrap().greeting(), "from matrix");
        assert_eq!(set.get(Some("email")).unwrap().greeting(), "from email");
        assert!(set.get(Some("slack")).is_none());
        // Bare request returns None when no default is set, even with
        // named providers registered.
        assert!(set.get(None).is_none());
        assert_eq!(set.provider_names(), vec!["email", "matrix"]);
    }

    #[test]
    fn capset_default_and_named_can_coexist() {
        // Default selection picks one named entry as the bare-request
        // resolution; the registry sets both pointers to the same Arc.
        let arc = Arc::new(Hello("primary"));
        let mut set: CapSet<dyn Greeter> = CapSet::new();
        set.default = Some(arc.clone());
        set.named.insert("primary".into(), arc);
        set.named
            .insert("backup".into(), Arc::new(Hello("secondary")));
        assert_eq!(set.get(None).unwrap().greeting(), "primary");
        assert_eq!(set.get(Some("primary")).unwrap().greeting(), "primary");
        assert_eq!(set.get(Some("backup")).unwrap().greeting(), "secondary");
    }

    // --- CapProvider ------------------------------------------------------

    struct NoopMessenger;
    impl Messenger for NoopMessenger {
        fn send<'a>(&'a self, _target: String, _body: MessageBody) -> CapFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    struct NoopMemory;
    impl MemoryAccess for NoopMemory {
        fn search<'a>(
            &'a self,
            _query: &'a str,
            _scope: MemoryScope,
        ) -> CapFuture<'a, Vec<MemoryHit>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn remember<'a>(
            &'a self,
            _key: &'a str,
            _value: &'a str,
            _scope: MemoryScope,
        ) -> CapFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    struct NoopAgentStateAdmin;
    impl AgentStateAdmin for NoopAgentStateAdmin {
        fn resolve_agent(&self, _name: &str) -> Result<DbEntry, String> {
            Err("not implemented".into())
        }
        fn open_agent_db<'a>(&'a self, _entry: &'a DbEntry) -> CapFuture<'a, AgentDb> {
            Box::pin(async { Err(anyhow::anyhow!("not implemented")) })
        }
    }

    #[test]
    fn cap_provider_kind_matches_variant() {
        let m: CapProvider = CapProvider::Messenger(Arc::new(NoopMessenger));
        assert_eq!(m.kind(), CapabilityKind::Messenger);
        let mem: CapProvider = CapProvider::Memory(Arc::new(NoopMemory));
        assert_eq!(mem.kind(), CapabilityKind::Memory);
        let a: CapProvider = CapProvider::AgentStateAdmin(Arc::new(NoopAgentStateAdmin));
        assert_eq!(a.kind(), CapabilityKind::AgentStateAdmin);
    }

    #[test]
    fn cap_provider_debug_redacts_arc_payload() {
        // Debug prints the Rust variant name (via derived `Debug` on
        // `CapabilityKind`); the snake_case form is only for `Display`
        // and the wire encoding.
        let m: CapProvider = CapProvider::Messenger(Arc::new(NoopMessenger));
        assert_eq!(format!("{m:?}"), "CapProvider(Messenger)");
        let mem: CapProvider = CapProvider::Memory(Arc::new(NoopMemory));
        assert_eq!(format!("{mem:?}"), "CapProvider(Memory)");
        let a: CapProvider = CapProvider::AgentStateAdmin(Arc::new(NoopAgentStateAdmin));
        assert_eq!(format!("{a:?}"), "CapProvider(AgentStateAdmin)");
    }

    // --- Data types -------------------------------------------------------

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
    fn session_entry_view_round_trip_preserves_shape() {
        let view = SessionEntryView {
            id: EntryId("abc123".into()),
            kind: "user_message".into(),
            data: serde_json::json!({"text": "hi"}),
            timestamp: Utc::now(),
        };
        let s = serde_json::to_string(&view).unwrap();
        let round: SessionEntryView = serde_json::from_str(&s).unwrap();
        assert_eq!(round, view);
    }

    // --- ExtensionCaps ----------------------------------------------------

    #[test]
    fn empty_extension_caps_grants_nothing() {
        let caps = ExtensionCaps::empty();
        assert!(caps.is_empty());
        assert!(caps.session_read.is_none());
        assert!(caps.session_write.is_none());
        assert!(caps.settings.is_none());
        assert!(caps.tool_registration.is_none());
        assert!(caps.command_registration.is_none());
        assert!(caps.messengers.is_empty());
        assert!(caps.memory.is_empty());
    }

    #[test]
    fn extension_caps_is_empty_flips_when_any_slot_filled() {
        let mut caps = ExtensionCaps::empty();
        caps.messengers.default = Some(Arc::new(NoopMessenger));
        assert!(!caps.is_empty(), "messenger default should count");

        let mut caps = ExtensionCaps::empty();
        caps.memory
            .named
            .insert("local".into(), Arc::new(NoopMemory));
        assert!(!caps.is_empty(), "named memory should count");
    }

    #[test]
    fn extension_caps_debug_summarizes_without_arc_payload() {
        let mut caps = ExtensionCaps::empty();
        caps.messengers
            .named
            .insert("matrix".into(), Arc::new(NoopMessenger));
        let s = format!("{caps:?}");
        // Slot booleans + provider names are present; raw Arc<dyn _>
        // pointers are not (they're not Debug-printable anyway).
        assert!(s.contains("session_read: false"), "got: {s}");
        assert!(s.contains("messengers: [\"matrix\"]"), "got: {s}");
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
