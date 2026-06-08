//! Memory tools — Memory Banks model.
//!
//! Two tools: `remember` / `recall`. Each takes an optional `bank`
//! argument. When absent, operates on the running agent's own
//! `AgentDb::memory` store (always accessible — the agent owns its own
//! DB). When present, looks the name up in the agent's `memory_banks`
//! subtree and operates on that bank's `memory` store; access is
//! gated by eidetica AuthSettings on the bank DB, authoritatively.
//!
//! There is no "global" scope. The older `MemoryGrant` capability type,
//! `global_remember`/`global_recall` tools, and the
//! `chaz_group.global_memory` store have been retired — anything
//! cross-agent is now a shared bank DB.

use crate::agent_db::MemoryEntry;
use crate::embedding::{Embedder, EmbeddingEntry, cosine_similarity, embeddings_store_name};
use crate::hosted_index::HostedIndex;
use crate::session::SessionRegistry;
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolPolicy};
use chrono::Utc;
use eidetica::Database;
use eidetica::store::Table;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, warn};

/// Shared helper: resolve the currently-running agent's `AgentDb` via the
/// index. Fails with a descriptive error if the agent has no DB on this
/// peer (e.g. imported without a key, or missing from the registry).
async fn open_own_agent_db(
    ctx: &ToolContext,
    registry: &SessionRegistry,
    index: &HostedIndex,
) -> Result<crate::agent_db::AgentDb, String> {
    let entry = index.find_by_name(&ctx.agent_name).ok_or_else(|| {
        format!(
            "Agent '{}' has no Living Agent DB on this peer",
            ctx.agent_name
        )
    })?;
    registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .map_err(|e| format!("Failed to open agent DB: {e}"))?
        .ok_or_else(|| {
            format!(
                "Peer holds no key for agent '{}' (DB {})",
                ctx.agent_name, entry.db_id
            )
        })
}

/// Extract a required string argument, returning a uniform error message.
fn str_arg<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing '{name}' argument"))
}

/// Schema for `remember`. Optional `bank` routes to a shared bank
/// instead of self-memory; optional `tags` attach free-form labels for
/// later recall filtering.
fn write_schema_banks() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "key":   { "type": "string", "description": "A short descriptive label for this fact (e.g. 'user_name', 'project_deadline')" },
            "value": { "type": "string", "description": "The fact to remember" },
            "tags":  { "type": "array", "items": { "type": "string" }, "description": "Optional: free-form labels for later filtering with recall (e.g. ['project', 'urgent'])." },
            "bank":  { "type": "string", "description": "Optional: name of a shared memory bank this agent has been granted Write access to. Omit to write to your own memory. Use the list_memory_banks tool to discover accessible banks." }
        },
        "required": ["key", "value"]
    })
}

/// Schema for `recall`. Ranks matches by BM25 over key + value + tags;
/// optional `tags` AND-filter narrows to entries carrying every listed
/// tag; optional `limit` caps the result count.
fn read_schema_banks() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Keywords to search for. Tokenized and ranked against entry keys, values, and tags. Pass an empty string to list entries by recency (useful with tags)." },
            "tags":  { "type": "array", "items": { "type": "string" }, "description": "Optional: only return entries that carry every listed tag." },
            "limit": { "type": "integer", "description": "Optional: cap the number of returned entries (default 10).", "minimum": 1 },
            "bank":  { "type": "string", "description": "Optional: name of a memory bank this agent has been granted Read access to. Omit to search your own memory. Use the list_memory_banks tool to discover accessible banks." }
        },
        "required": ["query"]
    })
}

/// Default cap on returned entries when the caller doesn't specify `limit`.
const DEFAULT_RECALL_LIMIT: usize = 10;

/// Parse `{key, value, tags?}`, write the entry to `(db, store)`, return the
/// success string. Shared by `Remember`'s self-memory and bank paths.
async fn do_remember(
    ctx: &ToolContext,
    arguments: &Value,
    db: &Database,
    store: &str,
    success_prefix: &str,
    log_scope: &'static str,
    embedder: Option<&dyn Embedder>,
) -> Result<String, String> {
    let key = str_arg(arguments, "key")?;
    let value = str_arg(arguments, "value")?;
    let tags = string_array_arg(arguments, "tags");
    let entry = MemoryEntry {
        key: key.to_string(),
        value: value.to_string(),
        timestamp: Utc::now(),
        tags,
    };
    write_memory_entry(db, store, entry, embedder).await?;
    debug!(agent = %ctx.agent_name, %key, scope = log_scope, "Stored memory");
    Ok(format!("{success_prefix}: {key} = {value}"))
}

/// Parse `{query, tags?, limit?}`, search `(db, store)`, return the formatted result.
/// Shared by `Recall`'s self-memory and bank paths.
async fn do_recall(
    ctx: &ToolContext,
    arguments: &Value,
    db: &Database,
    store: &str,
    log_scope: &'static str,
    embedder: Option<&dyn Embedder>,
) -> Result<String, String> {
    let query = str_arg(arguments, "query")?;
    let tags_filter = string_array_arg(arguments, "tags");
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_RECALL_LIMIT)
        .max(1);
    let result = search_memory(db, store, query, &tags_filter, limit, embedder).await?;
    debug!(agent = %ctx.agent_name, %query, scope = log_scope, "Recalled memory");
    Ok(result)
}

/// Pull `name` as a string array from `arguments`. Missing or malformed
/// values produce an empty vec — these are advisory, not load-bearing.
fn string_array_arg(arguments: &Value, name: &str) -> Vec<String> {
    arguments
        .get(name)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Store a fact in the running agent's own persistent memory.
pub struct Remember {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    embedder: Option<Arc<dyn Embedder>>,
}

impl Remember {
    pub fn new(
        registry: Arc<SessionRegistry>,
        agent_index: HostedIndex,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Self {
        Self {
            registry,
            agent_index,
            embedder,
        }
    }
}

impl Tool for Remember {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "remember".to_string(),
            description: "Store a fact in persistent memory. By default writes to this agent's own memory (travels with the agent via sync). Pass `bank` to write to a shared memory bank this agent has been granted Write access to — call list_memory_banks to discover options.".to_string(),
            parameters: write_schema_banks(),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let agent_db = open_own_agent_db(ctx, &self.registry, &self.agent_index).await?;
            let embedder = self.embedder.as_deref();
            match arguments.get("bank").and_then(|v| v.as_str()) {
                None => do_remember(
                    ctx,
                    &arguments,
                    agent_db.database(),
                    crate::agent_db::MEMORY_STORE,
                    "Remembered",
                    "own",
                    embedder,
                )
                .await
                .map_err(Into::into),
                Some(bank_name) => {
                    let bank =
                        resolve_bank_for_write(ctx, &agent_db, &self.registry, bank_name).await?;
                    do_remember(
                        ctx,
                        &arguments,
                        bank.database(),
                        crate::memory_bank_db::MEMORY_STORE,
                        &format!("Remembered in bank '{bank_name}'"),
                        "bank",
                        embedder,
                    )
                    .await
                    .map_err(Into::into)
                }
            }
        })
    }
}

/// Search the running agent's own memory for facts.
pub struct Recall {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    embedder: Option<Arc<dyn Embedder>>,
}

impl Recall {
    pub fn new(
        registry: Arc<SessionRegistry>,
        agent_index: HostedIndex,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Self {
        Self {
            registry,
            agent_index,
            embedder,
        }
    }
}

impl Tool for Recall {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "recall".to_string(),
            description: "Search persistent memory for previously stored facts. By default searches this agent's own memory. Pass `bank` to search a shared memory bank this agent has been granted Read access to — call list_memory_banks to discover options."
                .to_string(),
            parameters: read_schema_banks(),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let agent_db = open_own_agent_db(ctx, &self.registry, &self.agent_index).await?;
            let embedder = self.embedder.as_deref();
            match arguments.get("bank").and_then(|v| v.as_str()) {
                None => do_recall(
                    ctx,
                    &arguments,
                    agent_db.database(),
                    crate::agent_db::MEMORY_STORE,
                    "own",
                    embedder,
                )
                .await
                .map_err(Into::into),
                Some(bank_name) => {
                    let bank =
                        resolve_bank_for_read(ctx, &agent_db, &self.registry, bank_name).await?;
                    do_recall(
                        ctx,
                        &arguments,
                        bank.database(),
                        crate::memory_bank_db::MEMORY_STORE,
                        "bank",
                        embedder,
                    )
                    .await
                    .map_err(Into::into)
                }
            }
        })
    }
}

/// Look up `bank_name` in the running agent's `memory_banks` subtree
/// and open the bank DB. Confirms the ref exists and the recorded
/// permission is Write — for read-only refs, writes error out even if
/// eidetica would accept them (a defensive check; ultimate authority
/// still comes from the bank's AuthSettings).
async fn resolve_bank_for_write(
    ctx: &ToolContext,
    agent_db: &crate::agent_db::AgentDb,
    registry: &SessionRegistry,
    bank_name: &str,
) -> Result<crate::memory_bank_db::MemoryBankDb, String> {
    let bank_ref = match agent_db
        .find_memory_bank(bank_name)
        .await
        .map_err(|e| format!("Failed to look up memory bank: {e}"))?
    {
        Some(r) => r,
        None => return Err(unknown_bank_error(ctx, agent_db, bank_name).await),
    };

    if !matches!(bank_ref.permission, crate::agent_db::BankPermission::Write) {
        return Err(format!(
            "Memory bank '{bank_name}' is Read-only for this agent; cannot remember. \
             Ask the bank owner for Write access."
        ));
    }
    open_bank_by_ref(registry, &bank_ref).await
}

/// Look up `bank_name` and open the bank DB for a read. Read and Write
/// permissions both satisfy; only missing-ref and missing-key cases
/// fail.
async fn resolve_bank_for_read(
    ctx: &ToolContext,
    agent_db: &crate::agent_db::AgentDb,
    registry: &SessionRegistry,
    bank_name: &str,
) -> Result<crate::memory_bank_db::MemoryBankDb, String> {
    let bank_ref = match agent_db
        .find_memory_bank(bank_name)
        .await
        .map_err(|e| format!("Failed to look up memory bank: {e}"))?
    {
        Some(r) => r,
        None => return Err(unknown_bank_error(ctx, agent_db, bank_name).await),
    };
    open_bank_by_ref(registry, &bank_ref).await
}

async fn open_bank_by_ref(
    registry: &SessionRegistry,
    bank_ref: &crate::agent_db::MemoryBankRef,
) -> Result<crate::memory_bank_db::MemoryBankDb, String> {
    let db_id = eidetica::entry::ID::parse(&bank_ref.db_id)
        .map_err(|e| format!("Bank ref '{}' has invalid db_id: {e}", bank_ref.name))?;
    registry
        .open_memory_bank(&db_id, None)
        .await
        .map_err(|e| format!("Failed to open bank '{}': {e}", bank_ref.name))?
        .ok_or_else(|| {
            format!(
                "Memory bank '{}' is referenced but this peer holds no key for it (DB {}).",
                bank_ref.name, bank_ref.db_id
            )
        })
}

/// Build a helpful "bank not found" error listing what the agent *can*
/// see, so the LLM can self-correct without a separate discovery call.
async fn unknown_bank_error(
    ctx: &ToolContext,
    agent_db: &crate::agent_db::AgentDb,
    bank_name: &str,
) -> String {
    let banks = agent_db.list_memory_banks().await.unwrap_or_default();
    if banks.is_empty() {
        format!(
            "Agent '{}' has no memory bank named '{bank_name}', and no banks granted. \
             Pass no `bank` arg to use your own memory.",
            ctx.agent_name
        )
    } else {
        let names: Vec<String> = banks
            .iter()
            .map(|b| format!("{} ({:?})", b.name, b.permission))
            .collect();
        format!(
            "Agent '{}' has no memory bank named '{bank_name}'. Available: {}.",
            ctx.agent_name,
            names.join(", ")
        )
    }
}

/// Tool: list the memory banks the running agent has been granted
/// access to. Mirrors `describe_tool`'s on-demand discovery pattern —
/// the LLM calls this when it wants to know what banks exist beyond
/// self memory, then uses the names with `remember`/`recall`.
pub struct ListMemoryBanks {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
}

impl ListMemoryBanks {
    pub fn new(registry: Arc<SessionRegistry>, agent_index: HostedIndex) -> Self {
        Self {
            registry,
            agent_index,
        }
    }
}

impl Tool for ListMemoryBanks {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "list_memory_banks".to_string(),
            description: "List every shared memory bank this agent has been granted access to, with permission level. Always shows 'self' (your own memory). Use the names with remember/recall's optional `bank` argument.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
    }

    fn strict_schema(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        _arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, crate::tool::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let agent_db = open_own_agent_db(ctx, &self.registry, &self.agent_index).await?;
            let banks = agent_db
                .list_memory_banks()
                .await
                .map_err(|e| format!("Failed to list memory banks: {e}"))?;
            let mut lines = vec!["- **self** (Write) — your own memory".to_string()];
            for b in &banks {
                lines.push(format!(
                    "- **{}** ({:?}) — DB {}",
                    b.name, b.permission, b.db_id
                ));
            }
            Ok(lines.join("\n"))
        })
    }
}

/// Shared writer for both scopes. When an embedder is configured,
/// embeds `key + " " + value` and inserts an `EmbeddingEntry` into
/// `embeddings:<model_id>` in the **same transaction** as the memory
/// row, joined by the row ID `Table::insert` returns. Embedder failures
/// are logged at warn and degrade to lexical-only — a transient
/// embedding API issue must not lose the user's memory.
pub(crate) async fn write_memory_entry(
    database: &Database,
    store_name: &str,
    entry: MemoryEntry,
    embedder: Option<&dyn Embedder>,
) -> Result<(), String> {
    // Embed before opening the txn — the embedding call is async I/O
    // and we don't want a long-held write lock.
    let embed_text = format!("{} {}", entry.key, entry.value);
    let embedding = match embedder {
        Some(e) => match e.embed(&embed_text).await {
            Ok(v) => Some((e.model_id().to_string(), v)),
            Err(err) => {
                warn!(?err, model = %e.model_id(), "Embedding failed; storing memory without semantic vector");
                None
            }
        },
        None => None,
    };

    let txn = database
        .new_transaction()
        .await
        .map_err(|e| format!("Failed to create transaction: {e}"))?;
    let store = txn
        .get_store::<Table<MemoryEntry>>(store_name)
        .await
        .map_err(|e| format!("Failed to open memory store: {e}"))?;
    let row_id = store
        .insert(entry)
        .await
        .map_err(|e| format!("Failed to store memory: {e}"))?;

    if let Some((model_id, vector)) = embedding {
        let emb_store_name = embeddings_store_name(&model_id);
        let emb_store = txn
            .get_store::<Table<EmbeddingEntry>>(&emb_store_name)
            .await
            .map_err(|e| format!("Failed to open embedding store: {e}"))?;
        emb_store
            .insert(EmbeddingEntry {
                memory_row_id: row_id,
                vector,
            })
            .await
            .map_err(|e| format!("Failed to store embedding: {e}"))?;
    }

    txn.commit()
        .await
        .map_err(|e| format!("Failed to commit memory: {e}"))?;
    Ok(())
}

/// One entry plus its relevance score. Returned by
/// [`search_memory_scored`]; thin wrappers format it for tool callers
/// or map it onto a cap's `MemoryHit` for extension consumers.
#[derive(Debug, Clone)]
pub(crate) struct ScoredMemory {
    pub entry: MemoryEntry,
    /// RRF score when both rankers ran; raw BM25/cosine score when only
    /// one did; recency rank-position (descending) on the
    /// no-query/no-embedder browse path. Comparable only within a
    /// single result set.
    pub score: f32,
}

/// Search memory entries by hybrid lexical + semantic relevance,
/// optionally pre-filtered by tags. Returns the top `limit` formatted
/// as a Markdown list.
///
/// Thin Markdown-formatting wrapper over [`search_memory_scored`].
/// Used by the `recall` tool and the auto-recall context tail where
/// the consumer expects a ready-to-render string. Extension callers
/// that need structured data should call [`search_memory_structured`]
/// instead — it skips the format-then-parse round-trip.
pub(crate) async fn search_memory(
    database: &Database,
    store_name: &str,
    query: &str,
    tags_filter: &[String],
    limit: usize,
    embedder: Option<&dyn Embedder>,
) -> Result<String, String> {
    let hits =
        search_memory_scored(database, store_name, query, tags_filter, limit, embedder).await?;
    if hits.is_empty() {
        return Ok(no_results_message(query, tags_filter));
    }
    Ok(hits
        .iter()
        .map(|h| format_entry(&h.entry))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Structured counterpart to [`search_memory`]: returns the top-`limit`
/// scored entries without formatting. Empty result is `Ok(vec![])` —
/// callers compose their own no-results messaging.
pub(crate) async fn search_memory_structured(
    database: &Database,
    store_name: &str,
    query: &str,
    tags_filter: &[String],
    limit: usize,
    embedder: Option<&dyn Embedder>,
) -> Result<Vec<ScoredMemory>, String> {
    search_memory_scored(database, store_name, query, tags_filter, limit, embedder).await
}

/// Hybrid lexical + semantic search pipeline. The shared core that
/// both [`search_memory`] and [`search_memory_structured`] sit on top
/// of.
///
/// Pipeline:
/// 1. (Outside any txn) Embed the query if an embedder is configured.
///    Embedding errors degrade to lexical-only.
/// 2. Open a transaction. Load every entry from the `memory` store,
///    dedupe by `key` (most-recent-by-timestamp wins; older rows are
///    logically stale). Tracks the surviving row IDs so we can join
///    against the `embeddings:<model_id>` subtree.
/// 3. AND-filter by `tags_filter` (case-insensitive exact match per
///    tag).
/// 4. If `query` tokenizes to nothing AND no embedder, return the
///    surviving entries by recency.
/// 5. Compute BM25 ranks (over `key + value + tags`) and cosine ranks
///    (over the live embedding vectors). Combine with Reciprocal Rank
///    Fusion (k=60). Entries appearing in only one ranker still
///    surface.
async fn search_memory_scored(
    database: &Database,
    store_name: &str,
    query: &str,
    tags_filter: &[String],
    limit: usize,
    embedder: Option<&dyn Embedder>,
) -> Result<Vec<ScoredMemory>, String> {
    let trimmed_query = query.trim();
    // Embed the query first (skip on empty query; embedding "" is wasteful
    // and most providers reject it). Failures degrade to lexical-only.
    let query_embedding = match (embedder, trimmed_query.is_empty()) {
        (Some(e), false) => match e.embed(trimmed_query).await {
            Ok(v) => Some((e.model_id().to_string(), v)),
            Err(err) => {
                warn!(?err, model = %e.model_id(), "Query embedding failed; falling back to lexical-only");
                None
            }
        },
        _ => None,
    };

    let txn = database
        .new_transaction()
        .await
        .map_err(|e| format!("Failed to create transaction: {e}"))?;
    let store = txn
        .get_store::<Table<MemoryEntry>>(store_name)
        .await
        .map_err(|e| format!("Failed to open memory store: {e}"))?;

    let records = store
        .search(|_: &MemoryEntry| true)
        .await
        .map_err(|e| format!("Failed to search memory: {e}"))?;

    // Dedupe by key, keep the most recent row plus its row ID. The row
    // ID is the join key into `embeddings:<model_id>`.
    let mut by_key: std::collections::HashMap<String, (String, MemoryEntry)> =
        std::collections::HashMap::new();
    for (row_id, entry) in records {
        by_key
            .entry(entry.key.clone())
            .and_modify(|existing| {
                if entry.timestamp > existing.1.timestamp {
                    *existing = (row_id.clone(), entry.clone());
                }
            })
            .or_insert((row_id, entry));
    }
    let kept: Vec<(String, MemoryEntry)> = by_key
        .into_values()
        .filter(|(_, e)| entry_has_all_tags(e, tags_filter))
        .collect();

    if kept.is_empty() {
        return Ok(Vec::new());
    }

    // Side-load the embedding subtree if we have a query vector. Missing
    // subtree (e.g. nothing was ever embedded against this model) yields
    // an empty map; the recall pathway then degrades to BM25-only.
    let live_embeddings: std::collections::HashMap<String, Vec<f32>> = match &query_embedding {
        Some((model_id, _)) => {
            let emb_store_name = embeddings_store_name(model_id);
            match txn
                .get_store::<Table<EmbeddingEntry>>(&emb_store_name)
                .await
            {
                Ok(s) => match s.search(|_: &EmbeddingEntry| true).await {
                    Ok(rows) => rows
                        .into_iter()
                        .map(|(_, e)| (e.memory_row_id, e.vector))
                        .collect(),
                    Err(err) => {
                        warn!(?err, "Failed reading embedding store; using lexical-only");
                        std::collections::HashMap::new()
                    }
                },
                Err(err) => {
                    debug!(?err, store = %emb_store_name, "No embedding subtree on this DB");
                    std::collections::HashMap::new()
                }
            }
        }
        None => std::collections::HashMap::new(),
    };

    let query_tokens = tokenize(query);
    let entries: Vec<MemoryEntry> = kept.iter().map(|(_, e)| e.clone()).collect();

    let chosen: Vec<ScoredMemory> = if query_tokens.is_empty() && query_embedding.is_none() {
        // Plain "browse by tag/recency" path. Assign descending synthetic
        // scores so callers can still rank/compare; the absolute values
        // aren't meaningful across result sets.
        let mut sorted = entries;
        sorted.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        sorted.truncate(limit);
        let n = sorted.len();
        sorted
            .into_iter()
            .enumerate()
            .map(|(i, entry)| ScoredMemory {
                entry,
                score: (n - i) as f32 / n.max(1) as f32,
            })
            .collect()
    } else {
        let bm25_ranking = if query_tokens.is_empty() {
            Vec::new()
        } else {
            score_bm25(&entries, &query_tokens)
        };
        let cosine_ranking = match &query_embedding {
            Some((_, qv)) if !live_embeddings.is_empty() => {
                score_cosine(&kept, qv, &live_embeddings)
            }
            _ => Vec::new(),
        };
        if bm25_ranking.is_empty() && cosine_ranking.is_empty() {
            Vec::new()
        } else {
            rrf_combine(&entries, &bm25_ranking, &cosine_ranking, limit)
        }
    };

    Ok(chosen)
}

fn entry_has_all_tags(entry: &MemoryEntry, required: &[String]) -> bool {
    required.iter().all(|want| {
        entry
            .tags
            .iter()
            .any(|have| have.eq_ignore_ascii_case(want))
    })
}

fn format_entry(m: &MemoryEntry) -> String {
    if m.tags.is_empty() {
        format!(
            "- **{}**: {} ({})",
            m.key,
            m.value,
            m.timestamp.to_rfc3339()
        )
    } else {
        format!(
            "- **{}**: {} [tags: {}] ({})",
            m.key,
            m.value,
            m.tags.join(", "),
            m.timestamp.to_rfc3339()
        )
    }
}

fn no_results_message(query: &str, tags_filter: &[String]) -> String {
    match (query.trim().is_empty(), tags_filter.is_empty()) {
        (true, true) => "No memories stored.".to_string(),
        (true, false) => format!("No memories found with tags [{}].", tags_filter.join(", ")),
        (false, true) => format!("No memories found matching '{query}'."),
        (false, false) => format!(
            "No memories found matching '{query}' with tags [{}].",
            tags_filter.join(", ")
        ),
    }
}

/// Lowercase + split-on-non-alphanumeric tokenizer. No stemming or
/// stopword removal — keep it dep-free; revisit if recall quality
/// demands it. Tokens shorter than 2 chars are dropped to cut noise.
fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(String::from)
        .collect()
}

/// Score every entry in `entries` against `query_tokens` using BM25
/// (`k1=1.5`, `b=0.75`). Returns `(score, index)` sorted descending,
/// dropping entries that match no query term. Used both stand-alone and
/// as one input to RRF.
fn score_bm25(entries: &[MemoryEntry], query_tokens: &[String]) -> Vec<(f64, usize)> {
    const K1: f64 = 1.5;
    const B: f64 = 0.75;

    let docs: Vec<Vec<String>> = entries.iter().map(|e| tokenize(&doc_text(e))).collect();
    let n = docs.len() as f64;
    if n == 0.0 {
        return Vec::new();
    }
    let total_len: usize = docs.iter().map(|d| d.len()).sum();
    let avg_dl = (total_len as f64 / n).max(1.0);

    let mut df: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for doc in &docs {
        let unique: std::collections::HashSet<&str> = doc.iter().map(String::as_str).collect();
        for term in unique {
            *df.entry(term).or_insert(0) += 1;
        }
    }

    let mut scored: Vec<(f64, usize)> = docs
        .iter()
        .enumerate()
        .map(|(i, doc)| {
            let dl = doc.len() as f64;
            let mut score = 0.0_f64;
            for q in query_tokens {
                let q_str = q.as_str();
                let tf = doc.iter().filter(|t| t.as_str() == q_str).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let df_q = *df.get(q_str).unwrap_or(&0) as f64;
                let idf = ((n - df_q + 0.5) / (df_q + 0.5) + 1.0).ln();
                let norm = tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
                score += idf * norm;
            }
            (score, i)
        })
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|(a, _), (b, _)| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// Score the query vector against each entry that has a live embedding
/// (joined by row ID). Entries without an embedding are dropped (they
/// can still surface via BM25). Returns `(similarity, index)` sorted
/// descending, dropping non-positive similarities (well-formed
/// embeddings normally produce positive cosine for related text).
fn score_cosine(
    kept: &[(String, MemoryEntry)],
    query_vec: &[f32],
    live_embeddings: &std::collections::HashMap<String, Vec<f32>>,
) -> Vec<(f32, usize)> {
    let mut scored: Vec<(f32, usize)> = kept
        .iter()
        .enumerate()
        .filter_map(|(i, (row_id, _))| {
            live_embeddings
                .get(row_id)
                .map(|v| (cosine_similarity(query_vec, v), i))
        })
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|(a, _), (b, _)| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// Reciprocal Rank Fusion. Given two pre-sorted ranked lists indexed
/// into `entries`, produce up to `limit` entries by combined score
/// `1/(K + rank_bm25) + 1/(K + rank_cosine)`. Entries appearing in only
/// one list still surface (the missing rank contributes zero, not
/// negative weight). `K=60` is the conventional default from
/// Cormack/Clarke/Buettcher, "Reciprocal Rank Fusion outperforms Condorcet
/// and individual Rank Learning Methods" (SIGIR 2009).
fn rrf_combine(
    entries: &[MemoryEntry],
    bm25: &[(f64, usize)],
    cosine: &[(f32, usize)],
    limit: usize,
) -> Vec<ScoredMemory> {
    const K: f64 = 60.0;
    let mut scores: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    for (rank, (_, idx)) in bm25.iter().enumerate() {
        *scores.entry(*idx).or_insert(0.0) += 1.0 / (K + (rank as f64) + 1.0);
    }
    for (rank, (_, idx)) in cosine.iter().enumerate() {
        *scores.entry(*idx).or_insert(0.0) += 1.0 / (K + (rank as f64) + 1.0);
    }
    let mut combined: Vec<(usize, f64)> = scores.into_iter().collect();
    combined.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    combined.truncate(limit);
    combined
        .into_iter()
        .map(|(i, s)| ScoredMemory {
            entry: entries[i].clone(),
            score: s as f32,
        })
        .collect()
}

fn doc_text(entry: &MemoryEntry) -> String {
    let mut s = String::with_capacity(entry.key.len() + entry.value.len() + 8);
    s.push_str(&entry.key);
    s.push(' ');
    s.push_str(&entry.value);
    if !entry.tags.is_empty() {
        s.push(' ');
        s.push_str(&entry.tags.join(" "));
    }
    s
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
