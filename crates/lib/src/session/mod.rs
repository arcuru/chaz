//! Per-conversation state (`Session`) and the central registry
//! (`SessionRegistry`).
//!
//! A session is one conversation. Each owns a dedicated eidetica Database
//! with two stores:
//! - `entries` (Table<SessionEntry>) — message/directive/tool-call history
//! - `meta`    (DocStore)            — session configuration (name, agent, model, ...)
//!
//! The registry (inside `chaz_group`) holds only indices: `sessions`,
//! `matrix_channels`, `session_names`. Canonical per-session config lives
//! in each session's own DB (`SessionMeta`) so it syncs with the session.
//!
//! Submodules split `impl SessionRegistry` blocks by concern:
//! - `registry` — constructor, session CRUD, name index, accessors
//! - `channels` — Matrix room ↔ session bindings
//! - `agents`   — attach/detach agents + turn-taking resolution
//! - `keys`     — agent DB helpers + ephemeral key lifecycle

use crate::types::ConversationId;

use chrono::{DateTime, Utc};
use eidetica::Database;
use eidetica::store::{DocStore, Table};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

mod agents;
mod channels;
mod keys;
mod registry;
pub mod usage;

pub use keys::BootstrapOutcome;
#[cfg(test)]
mod test_helpers;

pub use registry::SessionRegistry;

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
    pub model: Option<String>,
    pub role_name: Option<String>,
    pub role_prompt: Option<String>,
    pub backend_name: Option<String>,
    pub backend_url: Option<String>,
    pub backend_key_ref: Option<String>,
}

/// Registry index entry — exists for every session known to this instance.
///
/// Combines the lightweight routing index (`sessions` DocStore: id→source)
/// with the richer catalog metadata (`session_catalog` DocStore: gateway,
/// created_at, status). Legacy sessions registered before the catalog
/// existed surface here with `gateway = Other` and `created_at = None`.
#[derive(Debug, Clone)]
pub struct SessionIndex {
    pub session_db_id: String,
    /// Free-form origin tag for debugging ("matrix:!room", "tui", "spawn:uuid").
    pub source: Option<String>,
    pub gateway: GatewayKind,
    pub created_at: Option<DateTime<Utc>>,
    pub status: SessionStatus,
}

/// Normalized gateway-of-origin derived from the session's `source` tag.
/// Stored alongside the raw source so consumers can filter without parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GatewayKind {
    Cli,
    Tui,
    Matrix,
    Spawn,
    #[default]
    Other,
}

impl GatewayKind {
    /// Map a free-form `source` tag to a normalized gateway kind.
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
    /// CLI filters (`chaz usage --gateway tui`).
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
    pub gateway: GatewayKind,
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
        };

        session.load_from_db().await;
        session
    }

    /// Load entries from eidetica
    async fn load_from_db(&mut self) {
        let Ok(txn) = self.database.new_transaction().await else {
            return;
        };
        if let Ok(store) = txn.get_store::<Table<SessionEntry>>(&self.store_name).await {
            match store.search(|_| true).await {
                Ok(records) => {
                    let mut entries: Vec<SessionEntry> =
                        records.into_iter().map(|(_, entry)| entry).collect();
                    entries.sort_by_key(|e| e.timestamp);
                    self.entries = entries;
                }
                Err(e) => error!("Failed to load session entries from eidetica: {e}"),
            }
        }
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

    /// Merge backfill history from a gateway (e.g., Matrix room history).
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

    SessionMeta {
        name: store.get_string("name").await.ok(),
        agent_name: store.get_string("agent_name").await.ok(),
        agents,
        host_agent_db_id: store.get_string("host_agent_db_id").await.ok(),
        model: store.get_string("model").await.ok(),
        role_name: store.get_string("role_name").await.ok(),
        role_prompt: store.get_string("role_prompt").await.ok(),
        backend_name: store.get_string("backend_name").await.ok(),
        backend_url: store.get_string("backend_url").await.ok(),
        backend_key_ref: store.get_string("backend_key_ref").await.ok(),
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
}
