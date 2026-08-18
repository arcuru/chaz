//! Per-conversation state (`Session`) and the central registry
//! (`SessionRegistry`).
//!
//! A session is one conversation. Each owns a dedicated eidetica Database
//! with two stores:
//! - `entries` (Table<SessionEntry>) — message/directive/tool-call history
//! - `meta`    (DocStore)            — session configuration (name, agent, model, ...)
//!
//! The registry (inside `chaz_group`) holds only indices: `sessions`,
//! `session_names`. Canonical per-session config lives in each session's own
//! DB (`SessionMeta`) so it syncs with the session — including its transport
//! bindings (`transport` module).
//!
//! Submodules split `impl SessionRegistry` blocks by concern:
//! - `registry`  — constructor, session CRUD, name index, accessors
//! - `transport` — channel ↔ session bindings (in the session DB) + lookup
//! - `agents`    — attach/detach agents + turn-taking resolution
//! - `keys`      — agent DB helpers + ephemeral key lifecycle

use crate::types::ConversationId;

use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::store::{DocStore, Table};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info, warn};

mod agents;
mod keys;
mod registry;
mod transport;
pub mod usage;

pub use keys::BootstrapOutcome;
#[cfg(test)]
pub(crate) mod test_helpers;

pub use registry::SessionRegistry;
pub use transport::{bind_transport, is_bound, transport_bindings, unbind_transport};

/// Type of session entry. Participants (users and agents alike) write entries
/// to a session. There is no user/agent distinction at the protocol level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntryType {
    /// A chat message (from any participant)
    Message,
    /// A task/instruction from a non-user source (spawn_agent, scheduler, system).
    /// Included in LLM context as a user message.
    Directive,
    /// Record of a tool invocation (audit trail). Excluded from LLM context.
    ToolCall,
    /// Record of a tool result (audit trail). Excluded from LLM context.
    ToolResult,
    /// Acknowledgement that work is in progress
    Ack,
    /// An error that occurred during processing
    Error,
    /// A compacted summary of older messages, written by /compact or the compact tool.
    /// Context builder treats the most recent Summary as the start boundary.
    Summary,
    /// A tool-approval request the daemon proxies over the session DB: the
    /// runtime blocks on it while a bridge renders the prompt and a human
    /// decides. Content is a JSON [`crate::bridge::ApprovalRequestPayload`].
    /// Excluded from LLM context and from bridge message delivery; never wakes
    /// an agent turn.
    ApprovalRequest,
    /// A human's decision on an [`EntryType::ApprovalRequest`], written by the
    /// bridge that captured the reaction/command. Content is a JSON
    /// [`crate::bridge::ApprovalDecisionPayload`]. The daemon's approval proxy
    /// matches it back to the blocked request by `request_id`. Same exclusions
    /// as `ApprovalRequest`.
    ApprovalDecision,
}

/// An entry in a session. Participants (human users and AI agents) are
/// treated identically — both write SessionEntries with their name as sender.
/// The agent determines assistant vs user roles at context-building time by
/// comparing the sender to its own name.
///
/// `metadata` carries token/cost provenance for assistant `Message` entries
/// (aggregated across the turn's ReAct loop). It is `None` for all other
/// entry kinds (human messages, directives, tool calls, tool results,
/// acks, and errors). Stored alongside the entry so cost attribution
/// survives session sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub sender: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub entry_type: EntryType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<crate::runtime::ResponseMetadata>,
    /// Transport routing provenance. Present on entries that entered or
    /// leave the session via an external bridge (Matrix, Discord, …);
    /// `None` for purely local entries (TUI/CLI chat, tool audit, summaries),
    /// which is the common case. Grouped into one optional field so the
    /// many [`SessionEntry`] construction sites stay a one-line `routing:
    /// None` and future routing fields add zero per-site churn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<EntryRouting>,
}

/// Transport routing metadata for a [`SessionEntry`].
///
/// The session DB is the only channel between bridges and the runtime: an
/// ingester stamps `source` on inbound user entries; a publisher (one per
/// login) scans outbound assistant entries and acts on those whose
/// `reply_to`/`destinations` resolve to its own `(transport, login_id)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EntryRouting {
    /// Set by the bridge ingester on inbound user messages: which
    /// transport/login/channel this entry arrived on, and from whom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<TransportRef>,
    /// Default-reply pointer: the transport `message_id` of the inbound
    /// entry this is a reply to. The publisher resolves the destination by
    /// finding the referenced entry and reading its `source`. Lets the
    /// runtime stay transport-agnostic for the chat-in → chat-out case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Explicit destinations for proactive / cross-transport sends. Each
    /// publisher sends the entries whose destination matches its own
    /// `(transport, login_id)`. Empty for the implicit reply-to-source path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<TransportRef>,
}

/// A transport-scoped address: where a message came from or should go.
///
/// `login_id` disambiguates two logins running on the same transport (one
/// shared across agents, one dedicated) — a publisher only acts on refs
/// matching its own login. See the bridge design doc for the full model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransportRef {
    /// Stable transport identifier — "matrix", "discord", "sms".
    pub transport: String,
    /// Which login received (source) / should send (destination) this.
    pub login_id: String,
    /// Transport-scoped channel address — Matrix room_id, Discord
    /// channel_id, phone number.
    pub channel: String,
    /// Sender of an inbound message (Matrix MXID, etc.). Set on `source`,
    /// unset on `destinations`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// Sender display name at receipt time, for natural addressing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_display: Option<String>,
    /// Transport-native message id, for threading / edits / reactions and
    /// as the target of another entry's `reply_to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// A reference to an agent authorized to participate in a session.
///
/// `db_id` is the agent's eidetica Database root ID — its global identity.
/// `display_name` caches the name so listings don't require opening the
/// agent's DB. Name is advisory; the DB id is canonical.
///
/// `home_pubkey` (per-session home peer): when set, only the peer whose
/// local key on the agent DB matches this pubkey will run the ReAct loop
/// for this agent in this session. `None` is the legacy default — any
/// keyholder runs (the multi-peer race the home-peer system exists to
/// fix). Set automatically on attach to the attacher's pubkey; rewritten
/// by `/agent rehost`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub db_id: String,
    pub display_name: String,
    #[serde(default)]
    pub home_pubkey: Option<String>,
}

/// Metadata stored in each session's own eidetica DB (under the "meta" DocStore).
///
/// This is the authoritative source for per-session configuration. It travels
/// with the session via eidetica sync — sharing a session also shares its
/// name, agent, model, role, and backend choices.
///
/// `agents` is the Living-Agents list of participating Agent DBs. The legacy
/// `agent_name` is still read for backward compatibility and as a fallback
/// when `agents` is empty; `agent_name` will be removed once all sessions
/// are migrated.
///
/// `host_agent_db_id` designates which agent answers when no @mention
/// pins the turn. Must be the `db_id` of an entry in `agents`; set via
/// `/agent host <ref>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub name: Option<String>,
    pub agent_name: Option<String>,
    #[serde(default)]
    pub agents: Vec<AgentRef>,
    pub host_agent_db_id: Option<String>,
    /// Session-wide model pin. Applies to every agent in the session
    /// unless overridden by an entry in `agent_models`.
    pub model: Option<String>,
    /// Per-agent session overrides keyed by `AgentRef.display_name`.
    /// An entry here beats `model` for that one agent; absence falls
    /// through to the agent's own `default_model` and then the backend.
    /// Stored as a JSON map in the meta DocStore under `agent_models`.
    #[serde(default)]
    pub agent_models: HashMap<String, String>,
    pub role_name: Option<String>,
    pub role_prompt: Option<String>,
    pub backend_name: Option<String>,
    pub backend_url: Option<String>,
    pub backend_key_ref: Option<String>,
    /// Per-session agent→agent burst budget override. `Some(n)` replaces
    /// the global `multi_agent.burst_budget`; `None` falls back to it.
    pub burst_budget_override: Option<usize>,
    /// Session-wide capability ceiling. Attenuates every tool call by every
    /// agent in this session — the outermost tier, above the agent-wide cap
    /// and per-tool grants. "This is a private, no-network session" lives
    /// here. Default (all-permissive) imposes no ceiling.
    #[serde(default)]
    pub capabilities: crate::grants::Grants,
}

impl SessionMeta {
    /// Resolve which model should run for `agent_name`. Order:
    /// per-agent override → session-wide pin. Returns `None` when neither
    /// is set so the caller can fall back to the agent's `default_model`
    /// and then the backend default.
    pub fn resolve_model_for_agent(&self, agent_name: &str) -> Option<&str> {
        self.agent_models
            .get(agent_name)
            .map(String::as_str)
            .or(self.model.as_deref())
    }
}

/// Registry index entry — exists for every session known to this instance.
///
/// Combines the lightweight routing index (`sessions` DocStore: id→source)
/// with the richer catalog metadata (`session_catalog` DocStore: bridge,
/// created_at, status). Legacy sessions registered before the catalog
/// existed surface here with `bridge = Other` and `created_at = None`.
#[derive(Debug, Clone)]
pub struct SessionIndex {
    pub session_db_id: String,
    /// Free-form origin tag for debugging ("matrix:!room", "tui", "spawn:uuid").
    pub source: Option<String>,
    pub bridge: BridgeKind,
    pub created_at: Option<DateTime<Utc>>,
    pub status: SessionStatus,
}

/// Normalized bridge-of-origin derived from the session's `source` tag.
/// Stored alongside the raw source so consumers can filter without parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BridgeKind {
    Cli,
    Tui,
    Matrix,
    Spawn,
    #[default]
    Other,
}

impl BridgeKind {
    /// Map a free-form `source` tag to a normalized bridge kind.
    pub fn from_source(source: Option<&str>) -> Self {
        match source {
            Some("cli") => Self::Cli,
            Some("tui") => Self::Tui,
            Some(s) if s.starts_with("matrix:") => Self::Matrix,
            Some(s) if s.starts_with("spawn:") => Self::Spawn,
            _ => Self::Other,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Tui => "tui",
            Self::Matrix => "matrix",
            Self::Spawn => "spawn",
            Self::Other => "other",
        }
    }

    /// Parse the case-insensitive short name produced by `as_str`. Used by
    /// CLI filters (`chaz usage --bridge tui`).
    pub fn from_filter_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cli" => Some(Self::Cli),
            "tui" => Some(Self::Tui),
            "matrix" => Some(Self::Matrix),
            "spawn" => Some(Self::Spawn),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionStatus {
    #[default]
    Active,
    Closed,
}

/// A row in the user-central session catalog.
///
/// Stored in `chaz_group`'s `session_catalog` DocStore (one entry per session
/// ever created on this peer). Caches only fields that don't drift after
/// creation — `name` and `agent_name` are intentionally NOT cached here, since
/// they live canonically in `SessionMeta` inside each session's own DB and
/// would require an update hook at every meta-write site.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCatalogEntry {
    pub session_db_id: String,
    pub source: Option<String>,
    /// Persisted to existing session DBs under the JSON key `gateway`; the
    /// `serde(rename)` preserves the on-disk format across the type rename.
    #[serde(rename = "gateway")]
    pub bridge: BridgeKind,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub status: SessionStatus,
}

/// Per-conversation state backed by its own eidetica Database.
pub struct Session {
    pub conversation_id: ConversationId,
    database: Database,
    entries: Vec<SessionEntry>,
    /// Store name — "entries" for regular, "entries:{id}" for ephemeral
    store_name: String,
    /// Set when loading existing history failed, so the session can be told
    /// apart from a genuinely new one. `None` means the load succeeded (which
    /// includes a new session with no entries yet).
    load_error: Option<eidetica::Error>,
}

const META_STORE: &str = "meta";

impl Session {
    /// Open a session, loading existing entries from its database.
    pub async fn new(conversation_id: ConversationId, database: Database) -> Self {
        let mut session = Session {
            conversation_id,
            database,
            entries: Vec::new(),
            store_name: "entries".to_string(),
            load_error: None,
        };

        session.load_from_db().await;
        session
    }

    /// Load entries from eidetica.
    ///
    /// A load failure is recorded in [`Self::load_error`] rather than left to
    /// look like an empty session: `get_store` registers a missing `entries`
    /// store and returns it empty, so a *new* session loads cleanly with zero
    /// entries, while an *existing* store whose history cannot be merged (two
    /// independently-rooted stores that share no common ancestor) fails the
    /// `search` and must be distinguishable.
    async fn load_from_db(&mut self) {
        let txn = match self.database.new_transaction().await {
            Ok(txn) => txn,
            Err(e) => {
                self.load_error = Some(e);
                return;
            }
        };
        let store = match txn.get_store::<Table<SessionEntry>>(&self.store_name).await {
            Ok(store) => store,
            Err(e) => {
                self.load_error = Some(e);
                return;
            }
        };
        match store.search(|_| true).await {
            Ok(records) => {
                let mut entries: Vec<SessionEntry> =
                    records.into_iter().map(|(_, entry)| entry).collect();
                entries.sort_by_key(|e| e.timestamp);
                self.entries = entries;
            }
            Err(e) => {
                error!("Failed to load session entries from eidetica: {e}");
                self.load_error = Some(e);
            }
        }
    }

    /// The error from the last history load, if one failed.
    ///
    /// `None` means the session's history loaded cleanly — including a
    /// genuinely new session with no entries. `Some` means entries are present
    /// in the store but could not be read (e.g. two peers created the `entries`
    /// store independently and the merged history shares no common ancestor),
    /// and [`Self::entries`] will report zero entries until the store is
    /// repaired. Callers should treat a session with a load error as unreadable
    /// rather than as a new, empty conversation.
    pub fn load_error(&self) -> Option<&eidetica::Error> {
        self.load_error.as_ref()
    }

    /// Add an entry to the session with persistence
    pub async fn add_entry(&mut self, entry: SessionEntry) {
        match self.database.new_transaction().await {
            Ok(txn) => match txn.get_store::<Table<SessionEntry>>(&self.store_name).await {
                Ok(store) => {
                    if let Err(e) = store.insert(entry.clone()).await {
                        error!("Failed to persist entry to eidetica: {e}");
                    } else if let Err(e) = txn.commit().await {
                        error!("Failed to commit to eidetica: {e}");
                    }
                }
                Err(e) => error!("Failed to open eidetica store: {e}"),
            },
            Err(e) => error!("Failed to create eidetica transaction: {e}"),
        }

        self.entries.push(entry);
    }

    /// Merge backfill history from a bridge (e.g., Matrix room history).
    /// Only inserts entries that are older than our earliest entry or fill gaps.
    /// Deduplicates by timestamp+content.
    pub async fn backfill(&mut self, history: Vec<SessionEntry>) {
        if history.is_empty() {
            return;
        }

        let mut new_count = 0;
        for entry in history {
            let already_exists = self.entries.iter().any(|existing| {
                existing.timestamp == entry.timestamp && existing.content == entry.content
            });
            if !already_exists {
                if let Ok(txn) = self.database.new_transaction().await
                    && let Ok(store) = txn.get_store::<Table<SessionEntry>>(&self.store_name).await
                    && store.insert(entry.clone()).await.is_ok()
                {
                    let _ = txn.commit().await;
                }
                self.entries.push(entry);
                new_count += 1;
            }
        }

        if new_count > 0 {
            self.entries.sort_by_key(|e| e.timestamp);
            info!(
                "Backfilled {} entries for {}",
                new_count, self.conversation_id
            );
        }
    }

    /// Get the most recent entry, if any
    pub fn latest_entry(&self) -> Option<&SessionEntry> {
        self.entries.last()
    }

    /// Get all entries in the session
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Get the underlying eidetica Database handle (for sharing with tools)
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Read session metadata from the session's own DB.
    /// Returns `SessionMeta::default()` if no meta has been written yet.
    pub async fn read_meta(&self) -> SessionMeta {
        read_meta_from_db(&self.database).await
    }

    /// Mutate session metadata in the session's own DB.
    /// The closure receives the current meta (default if unset) and may modify it.
    pub async fn update_meta<F>(&self, mutator: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut SessionMeta),
    {
        update_meta_on_db(&self.database, mutator).await
    }
}

/// Read the meta DocStore of a session DB. Returns default on any error.
pub async fn read_meta_from_db(database: &Database) -> SessionMeta {
    let Ok(txn) = database.new_transaction().await else {
        return SessionMeta::default();
    };
    let Ok(store) = txn.get_store::<DocStore>(META_STORE).await else {
        return SessionMeta::default();
    };

    let agents: Vec<AgentRef> = match store.get_string("agents").await {
        Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
            warn!("Malformed agents list in SessionMeta, ignoring: {e}");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    };

    let agent_models: HashMap<String, String> = match store.get_string("agent_models").await {
        Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
            warn!("Malformed agent_models map in SessionMeta, ignoring: {e}");
            HashMap::new()
        }),
        Err(_) => HashMap::new(),
    };

    let burst_budget_override: Option<usize> = match store.get_string("burst_budget_override").await
    {
        Ok(s) => s.parse().ok(),
        Err(_) => None,
    };

    let capabilities: crate::grants::Grants = match store.get_string("capabilities").await {
        Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
            warn!("Malformed capabilities in SessionMeta, ignoring: {e}");
            Default::default()
        }),
        Err(_) => Default::default(),
    };

    SessionMeta {
        name: store.get_string("name").await.ok(),
        agent_name: store.get_string("agent_name").await.ok(),
        agents,
        host_agent_db_id: store.get_string("host_agent_db_id").await.ok(),
        model: store.get_string("model").await.ok(),
        agent_models,
        role_name: store.get_string("role_name").await.ok(),
        role_prompt: store.get_string("role_prompt").await.ok(),
        backend_name: store.get_string("backend_name").await.ok(),
        backend_url: store.get_string("backend_url").await.ok(),
        backend_key_ref: store.get_string("backend_key_ref").await.ok(),
        burst_budget_override,
        capabilities,
    }
}

/// Apply a mutator to the meta DocStore of a session DB and commit.
pub async fn update_meta_on_db<F>(database: &Database, mutator: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut SessionMeta),
{
    let mut current = read_meta_from_db(database).await;
    mutator(&mut current);

    let txn = database.new_transaction().await?;
    let store = txn.get_store::<DocStore>(META_STORE).await?;

    write_field(&store, "name", current.name.as_deref()).await?;
    write_field(&store, "agent_name", current.agent_name.as_deref()).await?;
    if current.agents.is_empty() {
        let _ = store.delete("agents").await;
    } else {
        let json = serde_json::to_string(&current.agents)?;
        store.set_string("agents", json).await?;
    }
    write_field(
        &store,
        "host_agent_db_id",
        current.host_agent_db_id.as_deref(),
    )
    .await?;
    write_field(&store, "model", current.model.as_deref()).await?;
    if current.agent_models.is_empty() {
        let _ = store.delete("agent_models").await;
    } else {
        let json = serde_json::to_string(&current.agent_models)?;
        store.set_string("agent_models", json).await?;
    }
    write_field(&store, "role_name", current.role_name.as_deref()).await?;
    write_field(&store, "role_prompt", current.role_prompt.as_deref()).await?;
    write_field(&store, "backend_name", current.backend_name.as_deref()).await?;
    write_field(&store, "backend_url", current.backend_url.as_deref()).await?;
    write_field(
        &store,
        "backend_key_ref",
        current.backend_key_ref.as_deref(),
    )
    .await?;
    write_field(
        &store,
        "burst_budget_override",
        current
            .burst_budget_override
            .map(|n| n.to_string())
            .as_deref(),
    )
    .await?;
    if current.capabilities == crate::grants::Grants::default() {
        let _ = store.delete("capabilities").await;
    } else {
        let json = serde_json::to_string(&current.capabilities)?;
        store.set_string("capabilities", json).await?;
    }

    txn.commit().await?;
    Ok(())
}

async fn write_field(store: &DocStore, key: &str, value: Option<&str>) -> anyhow::Result<()> {
    match value {
        Some(v) => {
            store.set_string(key, v).await?;
        }
        None => {
            // Ignore KeyNotFound on delete — just means it wasn't set.
            let _ = store.delete(key).await;
        }
    }
    Ok(())
}

/// Find the most recent `Message` entry and produce a short single-line
/// preview ("sender: first line of content…") suitable for session listings.
/// Returns `None` if no `Message` entry exists. Shared between the
/// `list_sessions()` cold path and the TUI picker's row-patch warm path so
/// both code paths produce identical previews.
pub fn summarize_last_message(entries: &[SessionEntry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|e| e.entry_type == EntryType::Message)
        .map(|e| {
            let preview = e.content.lines().next().unwrap_or("");
            let truncated = crate::util::truncate_chars(preview, 60);
            if truncated.len() < preview.len() {
                format!("{}: {truncated}…", e.sender)
            } else {
                format!("{}: {preview}", e.sender)
            }
        })
}

/// What a session's own entries say about where it began: the timestamp of
/// its earliest entry, and the `"{transport}:{channel}"` source tag of the
/// first entry that arrived over a bridge.
///
/// The catalog caches both at creation, which works for a session this peer
/// created and not at all for one adopted from another peer: the adopting
/// peer knows only when *it* first saw the session, and re-derives that on
/// every adoption. Entries carry the real values and never drift, so an
/// adopted row can be seeded — and corrected — from them.
///
/// `entries` is expected in timestamp order, as [`Session::entries`] returns
/// it. Returns `(started_at, source)`, either of which is `None` when the
/// entries carry no evidence: an empty session, or one with no bridge-routed
/// entry (a purely local TUI/CLI conversation).
pub fn session_origin(entries: &[SessionEntry]) -> (Option<DateTime<Utc>>, Option<String>) {
    let started_at = entries.iter().map(|e| e.timestamp).min();
    let source = entries
        .iter()
        .find_map(|e| e.routing.as_ref()?.source.as_ref())
        .map(|s| format!("{}:{}", s.transport, s.channel));
    (started_at, source)
}

/// Sum `ResponseMetadata.usage.cost_usd` across an in-memory entry slice.
/// Returns `(total_cost_usd, cost_reported, llm_call_count)`.
///
/// Shared between `list_sessions()` (which walks every session's entries on
/// catalog open) and the TUI's per-row cache-patch path (which recomputes
/// just one row's totals when a watched session DB fires `on_write`). Both
/// see the same in-memory entries, so the cache stays in lock-step with
/// what `list_sessions()` would have produced from a cold read.
pub fn sum_session_cost(entries: &[SessionEntry]) -> (f64, bool, u32) {
    let mut total = 0.0_f64;
    let mut reported = false;
    let mut calls = 0u32;
    for entry in entries {
        let Some(m) = &entry.metadata else { continue };
        calls += 1;
        if let Some(c) = m.usage.cost_usd {
            total += c;
            reported = true;
        }
    }
    (total, reported, calls)
}

/// Extract `@<token>` mentions from free-form text. Returns the tokens
/// without the leading `@`, in appearance order. Tokens are split on
/// whitespace; punctuation directly adjacent to a mention is trimmed
/// from the tail (`@alpha,` → `alpha`).
pub fn parse_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        let Some(rest) = token.strip_prefix('@') else {
            continue;
        };
        let trimmed: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
            .collect();
        if !trimmed.is_empty() {
            out.push(trimmed);
        }
    }
    out
}

/// Count the trailing run of agent-authored `Message` entries, i.e. the
/// length of the current agent→agent "burst". The run is broken (and the
/// burst considered reset) by the first human-authored `Message` or any
/// `Directive` (scheduler/system) walking backward from the latest entry.
/// Non-conversational entries (`Ack`, `ToolCall`, `ToolResult`, `Error`,
/// `Summary`) are transparent — they neither extend nor
/// reset the burst.
///
/// `is_agent` decides whether a sender name belongs to a known agent.
/// Used to bound mention-chained agent→agent recursion: once the trailing
/// burst reaches the budget, further agent wakes are suppressed until a
/// human (or Directive) speaks.
pub fn trailing_agent_message_burst(
    entries: &[SessionEntry],
    is_agent: impl Fn(&str) -> bool,
) -> usize {
    let mut burst = 0;
    for e in entries.iter().rev() {
        match e.entry_type {
            EntryType::Message => {
                if is_agent(&e.sender) {
                    burst += 1;
                } else {
                    break; // human message — burst boundary
                }
            }
            EntryType::Directive => break, // scheduler/system — burst boundary
            _ => {}                        // transparent to the burst
        }
    }
    burst
}

/// Find or create a named eidetica database for a user.
async fn find_or_create_db(
    user: &mut eidetica::user::User,
    name: &str,
) -> anyhow::Result<Database> {
    match user.find_database(name).await {
        Ok(existing) if !existing.is_empty() => Ok(existing.into_iter().next().unwrap()),
        _ => {
            let mut settings = eidetica::crdt::Doc::new();
            settings.set("name", name);
            let key_id = user.get_default_key()?;
            Ok(user.create_database(settings, &key_id).await?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::*;
    use super::*;

    fn entry_at(sender: &str, minutes_ago: i64, routing: Option<EntryRouting>) -> SessionEntry {
        SessionEntry {
            sender: sender.to_string(),
            content: "hi".to_string(),
            timestamp: Utc::now() - chrono::Duration::minutes(minutes_ago),
            entry_type: EntryType::Message,
            metadata: None,
            routing,
        }
    }

    fn matrix_source(room: &str) -> EntryRouting {
        EntryRouting {
            source: Some(TransportRef {
                transport: "matrix".to_string(),
                login_id: "@ava:example.com".to_string(),
                channel: room.to_string(),
                sender: Some("@patrick:example.com".to_string()),
                sender_display: None,
                message_id: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn session_origin_reads_start_and_transport_from_entries() {
        let entries = vec![
            entry_at(
                "@patrick:example.com",
                90,
                Some(matrix_source("!room:example.com")),
            ),
            entry_at("ava", 89, None),
        ];

        let (started_at, source) = session_origin(&entries);
        assert_eq!(started_at, Some(entries[0].timestamp));
        assert_eq!(source.as_deref(), Some("matrix:!room:example.com"));
        // The tag has to be one `BridgeKind::from_source` actually recognizes,
        // or the picker still renders the session as `[other]`.
        assert_eq!(
            BridgeKind::from_source(source.as_deref()),
            BridgeKind::Matrix
        );
    }

    #[test]
    fn session_origin_takes_the_earliest_entry_not_the_first_routed_one() {
        // A local entry can precede the first bridge-routed one; the session
        // still started at the local entry.
        let entries = vec![
            entry_at("ava", 200, None),
            entry_at(
                "@patrick:example.com",
                100,
                Some(matrix_source("!room:example.com")),
            ),
        ];

        let (started_at, source) = session_origin(&entries);
        assert_eq!(started_at, Some(entries[0].timestamp));
        assert_eq!(source.as_deref(), Some("matrix:!room:example.com"));
    }

    #[test]
    fn session_origin_yields_nothing_without_evidence() {
        assert_eq!(session_origin(&[]), (None, None));

        let local_only = vec![entry_at("ava", 5, None)];
        let (started_at, source) = session_origin(&local_only);
        assert_eq!(started_at, Some(local_only[0].timestamp));
        assert_eq!(source, None, "a purely local session names no transport");
    }

    #[tokio::test]
    async fn session_meta_agents_round_trip() {
        let (_instance, _user, db) = test_session_db().await;

        let agents = vec![
            AgentRef {
                db_id: "sha256:abc".to_string(),
                display_name: "alpha".to_string(),
                home_pubkey: None,
            },
            AgentRef {
                db_id: "sha256:def".to_string(),
                display_name: "beta".to_string(),
                home_pubkey: None,
            },
        ];

        let expected = agents.clone();
        update_meta_on_db(&db, |m| m.agents = agents).await.unwrap();

        let read_back = read_meta_from_db(&db).await;
        assert_eq!(read_back.agents, expected);
    }

    #[tokio::test]
    async fn session_meta_capabilities_round_trip() {
        let (_instance, _user, db) = test_session_db().await;

        // Default (permissive) capabilities persist nothing and read back default.
        let read_back = read_meta_from_db(&db).await;
        assert_eq!(read_back.capabilities, crate::grants::Grants::default());

        // A session ceiling restricting egress to one domain round-trips intact.
        update_meta_on_db(&db, |m| {
            m.capabilities = crate::grants::Grants {
                network: Some(crate::grants::NetworkGrant {
                    endpoints: crate::grants::Allowlist::Only(vec![
                        crate::grants::EndpointPattern {
                            host: "*.corp.internal".to_string(),
                            path_prefix: None,
                            methods: None,
                        },
                    ]),
                    allow_private: true,
                }),
                ..Default::default()
            };
        })
        .await
        .unwrap();

        let read_back = read_meta_from_db(&db).await;
        let net = read_back.capabilities.network.expect("network ceiling");
        let endpoints = net.endpoints.entries().expect("not permissive");
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].host, "*.corp.internal");
        assert!(net.allow_private);

        // Resetting to default clears the stored field.
        update_meta_on_db(&db, |m| m.capabilities = Default::default())
            .await
            .unwrap();
        let read_back = read_meta_from_db(&db).await;
        assert_eq!(read_back.capabilities, crate::grants::Grants::default());
    }

    #[tokio::test]
    async fn session_meta_empty_agents_clears_field() {
        let (_instance, _user, db) = test_session_db().await;

        // Populate then clear.
        update_meta_on_db(&db, |m| {
            m.agents.push(AgentRef {
                db_id: "sha256:x".to_string(),
                display_name: "alpha".to_string(),
                home_pubkey: None,
            });
        })
        .await
        .unwrap();

        update_meta_on_db(&db, |m| m.agents.clear()).await.unwrap();

        let read_back = read_meta_from_db(&db).await;
        assert!(read_back.agents.is_empty());
    }

    #[tokio::test]
    async fn session_meta_coexists_with_agent_name() {
        let (_instance, _user, db) = test_session_db().await;
        update_meta_on_db(&db, |m| {
            m.agent_name = Some("legacy".to_string());
            m.agents.push(AgentRef {
                db_id: "sha256:a".to_string(),
                display_name: "modern".to_string(),
                home_pubkey: None,
            });
        })
        .await
        .unwrap();

        let meta = read_meta_from_db(&db).await;
        assert_eq!(meta.agent_name.as_deref(), Some("legacy"));
        assert_eq!(meta.agents.len(), 1);
        assert_eq!(meta.agents[0].display_name, "modern");
    }

    #[tokio::test]
    async fn session_meta_agent_models_round_trip() {
        let (_instance, _user, db) = test_session_db().await;

        update_meta_on_db(&db, |m| {
            m.model = Some("anthropic/claude-opus-4.7".to_string());
            m.agent_models
                .insert("researcher".to_string(), "ring-1t".to_string());
            m.agent_models
                .insert("ava".to_string(), "anthropic/claude-opus-4.7".to_string());
        })
        .await
        .unwrap();

        let meta = read_meta_from_db(&db).await;
        assert_eq!(meta.model.as_deref(), Some("anthropic/claude-opus-4.7"));
        assert_eq!(
            meta.agent_models.get("researcher").map(String::as_str),
            Some("ring-1t")
        );
        assert_eq!(meta.agent_models.len(), 2);
    }

    #[tokio::test]
    async fn session_meta_emptied_agent_models_clears_field() {
        let (_instance, _user, db) = test_session_db().await;

        update_meta_on_db(&db, |m| {
            m.agent_models.insert("ava".to_string(), "opus".to_string());
        })
        .await
        .unwrap();
        update_meta_on_db(&db, |m| m.agent_models.clear())
            .await
            .unwrap();

        let meta = read_meta_from_db(&db).await;
        assert!(meta.agent_models.is_empty());
    }

    #[test]
    fn resolve_model_for_agent_precedence() {
        // Per-agent override beats session pin. Absence falls through to
        // the session pin. Both absent returns None so the runtime can
        // fall back to the agent default and then the backend.
        let mut meta = SessionMeta {
            model: Some("session-pin".to_string()),
            ..Default::default()
        };
        meta.agent_models
            .insert("researcher".to_string(), "ring-1t".to_string());

        assert_eq!(meta.resolve_model_for_agent("researcher"), Some("ring-1t"));
        assert_eq!(meta.resolve_model_for_agent("ava"), Some("session-pin"));

        meta.model = None;
        assert_eq!(meta.resolve_model_for_agent("ava"), None);
        assert_eq!(meta.resolve_model_for_agent("researcher"), Some("ring-1t"));
    }

    #[test]
    fn agent_ref_deserializes_legacy_blob_without_home_pubkey() {
        // Pre-home_pubkey JSON shape: agents that were serialized before
        // the field existed must still deserialize with home_pubkey = None
        // (the `#[serde(default)]` attribute).
        let legacy = r#"{"db_id":"sha256:abc","display_name":"alpha"}"#;
        let parsed: AgentRef = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.db_id, "sha256:abc");
        assert_eq!(parsed.display_name, "alpha");
        assert_eq!(parsed.home_pubkey, None);
    }

    #[test]
    fn agent_ref_round_trips_with_home_pubkey_set() {
        let original = AgentRef {
            db_id: "sha256:def".to_string(),
            display_name: "beta".to_string(),
            home_pubkey: Some("ed25519:AbCdEf".to_string()),
        };
        let s = serde_json::to_string(&original).unwrap();
        let parsed: AgentRef = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn parse_mentions_basic() {
        assert_eq!(
            parse_mentions("hey @alpha can you help @beta?"),
            vec!["alpha", "beta"]
        );
        assert!(parse_mentions("no mentions here").is_empty());
        assert_eq!(parse_mentions("email a@b.com only"), Vec::<String>::new());
        assert_eq!(parse_mentions("@alpha-bot,"), vec!["alpha-bot"]);
    }

    #[test]
    fn trailing_agent_burst_counts_and_resets() {
        use chrono::Utc;

        let agents = ["alpha", "beta"];
        let is_agent = |name: &str| agents.contains(&name);
        let mk = |sender: &str, ty: EntryType| SessionEntry {
            sender: sender.to_string(),
            content: String::new(),
            timestamp: Utc::now(),
            entry_type: ty,
            metadata: None,
            routing: None,
        };

        // Empty / no trailing agent messages.
        assert_eq!(trailing_agent_message_burst(&[], is_agent), 0);
        assert_eq!(
            trailing_agent_message_burst(&[mk("patrick", EntryType::Message)], is_agent),
            0
        );

        // human → alpha → beta → alpha : burst of 3, human resets the run.
        let convo = vec![
            mk("patrick", EntryType::Message),
            mk("alpha", EntryType::Message),
            mk("beta", EntryType::Message),
            mk("alpha", EntryType::Message),
        ];
        assert_eq!(trailing_agent_message_burst(&convo, is_agent), 3);

        // Ack / ToolCall are transparent — don't reset.
        let with_noise = vec![
            mk("alpha", EntryType::Message),
            mk("server", EntryType::Ack),
            mk("alpha", EntryType::ToolCall),
            mk("beta", EntryType::Message),
        ];
        assert_eq!(trailing_agent_message_burst(&with_noise, is_agent), 2);

        // A Directive (scheduler/system) is a burst boundary.
        let after_directive = vec![
            mk("alpha", EntryType::Message),
            mk("scheduler", EntryType::Directive),
            mk("beta", EntryType::Message),
        ];
        assert_eq!(trailing_agent_message_burst(&after_directive, is_agent), 1);

        // Trailing human message → burst is 0 (handled via the human path).
        let human_last = vec![
            mk("alpha", EntryType::Message),
            mk("patrick", EntryType::Message),
        ];
        assert_eq!(trailing_agent_message_burst(&human_last, is_agent), 0);
    }

    /// Entries persisted before the `routing` field existed have no
    /// `routing` key in their stored JSON. They must still deserialize
    /// (to `routing: None`) so the schema change is backward-compatible
    /// with already-synced session DBs.
    #[test]
    fn legacy_entry_without_routing_deserializes_to_none() {
        let legacy = r#"{
            "sender": "patrick",
            "content": "hi",
            "timestamp": "2026-06-08T00:00:00Z",
            "entry_type": "Message"
        }"#;
        let entry: SessionEntry = serde_json::from_str(legacy).unwrap();
        assert!(entry.routing.is_none());
        assert_eq!(entry.sender, "patrick");
    }

    /// A local entry (no routing) serializes without a `routing` key —
    /// `skip_serializing_if` keeps the common case from bloating, and the
    /// absence is what the legacy-read test above relies on.
    #[test]
    fn entry_without_routing_omits_the_field() {
        let entry = SessionEntry {
            sender: "patrick".to_string(),
            content: "hi".to_string(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("routing"), "got: {json}");
    }

    /// An inbound transport entry round-trips its `source` provenance,
    /// including the `login_id` that distinguishes shared from dedicated
    /// logins on the same transport.
    #[test]
    fn entry_with_source_round_trips() {
        let entry = SessionEntry {
            sender: "@alice:example.com".to_string(),
            content: "hi".to_string(),
            timestamp: Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: Some(EntryRouting {
                source: Some(TransportRef {
                    transport: "matrix".to_string(),
                    login_id: "@bot:example.com".to_string(),
                    channel: "!room:example.com".to_string(),
                    sender: Some("@alice:example.com".to_string()),
                    sender_display: None,
                    message_id: Some("$evt".to_string()),
                }),
                reply_to: None,
                destinations: Vec::new(),
            }),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&json).unwrap();
        let src = back.routing.unwrap().source.unwrap();
        assert_eq!(src.transport, "matrix");
        assert_eq!(src.login_id, "@bot:example.com");
        assert_eq!(src.channel, "!room:example.com");
        assert_eq!(src.message_id.as_deref(), Some("$evt"));
    }

    #[tokio::test]
    async fn fresh_session_loads_with_no_error() {
        let (_instance, _user, db) = test_session_db().await;
        let session = Session::new(ConversationId(db.root_id().to_string()), db).await;
        assert!(session.entries().is_empty());
        assert!(
            session.load_error().is_none(),
            "a new session has no history, so it is not a load failure"
        );
    }

    /// Two transactions each first-create the `entries` store off the same
    /// pre-store base, so the merged history shares no common ancestor.
    /// Opening a `Session` over that DB must report the load failure instead
    /// of silently reading as a blank (new) session.
    #[tokio::test]
    async fn load_from_db_reports_failure_when_entries_store_cannot_merge() {
        let (_instance, _user, db) = test_session_db().await;

        // Both transactions snapshot the DB before the `entries` store exists.
        let tx_a = db.new_transaction().await.unwrap();
        let tx_b = db.new_transaction().await.unwrap();

        let store_a = tx_a
            .get_store::<Table<SessionEntry>>("entries")
            .await
            .unwrap();
        store_a.insert(entry_at("ava", 5, None)).await.unwrap();
        tx_a.commit().await.unwrap();

        let store_b = tx_b
            .get_store::<Table<SessionEntry>>("entries")
            .await
            .unwrap();
        store_b.insert(entry_at("ava", 4, None)).await.unwrap();
        tx_b.commit().await.unwrap();

        let session = Session::new(ConversationId(db.root_id().to_string()), db).await;
        assert!(
            session.entries().is_empty(),
            "an unmergeable history must not be read as a partial/empty session"
        );
        let err = session
            .load_error()
            .expect("load must report failure, not a blank session");
        assert!(
            err.to_string().contains("No common ancestor"),
            "unexpected error: {err}"
        );
    }
}
