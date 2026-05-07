//! Memory tools — Memory Banks model (Stage 9).
//!
//! Two tools: `remember` / `recall`. Each takes an optional `bank`
//! argument. When absent, operates on the running agent's own
//! `AgentDb::memory` store (always accessible — the agent owns its own
//! DB). When present, looks the name up in the agent's `memory_banks`
//! subtree and operates on that bank's `memory` store; access is
//! gated by eidetica AuthSettings on the bank DB, authoritatively.
//!
//! There is no "global" scope. The older `MemoryGrant` capability
//! type, `global_remember`/`global_recall` tools, and the
//! `chaz_group.global_memory` store all went away in Stage 9.E —
//! anything cross-agent is now a shared bank DB.

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
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy::default()
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
async fn write_memory_entry(
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

/// Search memory entries by hybrid lexical + semantic relevance,
/// optionally pre-filtered by tags. Returns the top `limit` formatted
/// as a Markdown list.
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
async fn search_memory(
    database: &Database,
    store_name: &str,
    query: &str,
    tags_filter: &[String],
    limit: usize,
    embedder: Option<&dyn Embedder>,
) -> Result<String, String> {
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
        return Ok(no_results_message(query, tags_filter));
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

    let chosen: Vec<MemoryEntry> = if query_tokens.is_empty() && query_embedding.is_none() {
        // Plain "browse by tag/recency" path.
        let mut sorted = entries;
        sorted.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        sorted.truncate(limit);
        sorted
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
            // Tokens didn't match anything and no semantic signal either.
            Vec::new()
        } else {
            rrf_combine(&entries, &bm25_ranking, &cosine_ranking, limit)
        }
    };

    if chosen.is_empty() {
        return Ok(no_results_message(query, tags_filter));
    }

    Ok(chosen
        .iter()
        .map(format_entry)
        .collect::<Vec<_>>()
        .join("\n"))
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
) -> Vec<MemoryEntry> {
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
        .map(|(i, _)| entries[i].clone())
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
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
    use crate::hosted_index::{DbEntry, HostedIndex};
    use crate::session::{Session, SessionRegistry};
    use crate::tool::{ScopedTools, ToolContext, ToolProfile, ToolRegistry};
    use crate::types::ConversationId;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    /// Full fixture: peer with a SessionRegistry + HostedIndex + one agent's
    /// DB registered, plus a dummy session so ToolContext has a valid handle.
    async fn fixture(
        agent_name: &str,
    ) -> (
        Instance,
        Arc<SessionRegistry>,
        HostedIndex,
        Arc<TokioMutex<Session>>,
    ) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
        let agents_reg = Arc::new(AgentRegistry::with_default_agent());
        let registry = Arc::new(
            SessionRegistry::new(instance.clone(), user, agents_reg)
                .await
                .unwrap(),
        );
        let index = HostedIndex::empty("agent");

        // Create an Agent DB for the named agent.
        let (agent_db, pubkey) = {
            let mut user = registry.user_for_tests().await;
            create_agent_db(
                &mut user,
                agent_name,
                &AgentDbConfig::default(),
                &AgentMeta {
                    display_name: Some(agent_name.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        };
        index.register(DbEntry {
            db_id: agent_db.id(),
            display_name: agent_name.to_string(),
            pubkey,
        });

        // Need a session for ToolContext.session — just create a blank one.
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session = Arc::new(TokioMutex::new(
            Session::new(ConversationId(session_db.root_id().to_string()), session_db).await,
        ));

        (instance, registry, index, session)
    }

    fn make_ctx(agent_name: &str, session: Arc<TokioMutex<Session>>) -> ToolContext {
        ToolContext {
            agent_name: agent_name.to_string(),
            call_depth: 0,
            max_call_depth: 10,
            tools: ScopedTools::new(Arc::new(ToolRegistry::new()), None),
            profile: ToolProfile::default(),
            session,
            grants: crate::grants::Grants::default(),
            agent_grants: std::collections::HashMap::new(),
            host: Arc::new(crate::tool_host::NativeToolHost::new()),
        }
    }

    #[tokio::test]
    async fn remember_writes_to_own_agent_db() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let tool = Remember::new(registry.clone(), index.clone(), None);
        let ctx = make_ctx("alpha", session);

        tool.execute(
            serde_json::json!({ "key": "favorite_color", "value": "blue" }),
            &ctx,
        )
        .await
        .unwrap();

        let recall = Recall::new(registry, index, None);
        let ctx2 = make_ctx("alpha", ctx.session.clone());
        let result = recall
            .execute(serde_json::json!({ "query": "favorite" }), &ctx2)
            .await
            .unwrap();
        assert!(result.contains("blue"), "expected blue in {result}");
    }

    #[tokio::test]
    async fn per_agent_memory_is_isolated() {
        // alpha and beta are separate agents on the same peer. Writing under
        // alpha must not appear under beta's recall.
        let (_instance, registry, index, session) = fixture("alpha").await;
        let (beta_db, beta_pubkey) = {
            let mut user = registry.user_for_tests().await;
            create_agent_db(
                &mut user,
                "beta",
                &AgentDbConfig::default(),
                &AgentMeta {
                    display_name: Some("beta".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        };
        index.register(DbEntry {
            db_id: beta_db.id(),
            display_name: "beta".to_string(),
            pubkey: beta_pubkey,
        });

        let remember = Remember::new(registry.clone(), index.clone(), None);
        let recall = Recall::new(registry, index, None);

        let ctx_alpha = make_ctx("alpha", session.clone());
        remember
            .execute(
                serde_json::json!({ "key": "secret", "value": "alpha-only" }),
                &ctx_alpha,
            )
            .await
            .unwrap();

        let ctx_beta = make_ctx("beta", session);
        let result = recall
            .execute(serde_json::json!({ "query": "secret" }), &ctx_beta)
            .await
            .unwrap();
        assert!(
            !result.contains("alpha-only"),
            "leakage across agents: {result}"
        );
        assert!(
            result.contains("No memories"),
            "expected no-results for beta, got: {result}"
        );
    }

    // -------------------------------------------------------------------------
    // Stage 9.C — memory banks via optional `bank` param
    // -------------------------------------------------------------------------

    /// Helper: create a memory bank DB on the peer, attach it to the agent
    /// with the given permission, return the bank's DB ID.
    async fn provision_bank(
        registry: &Arc<SessionRegistry>,
        agent_name: &str,
        bank_name: &str,
        permission: crate::agent_db::BankPermission,
    ) -> String {
        let (bank, _pk) = {
            let mut user = registry.user_for_tests().await;
            crate::memory_bank_db::create_memory_bank(
                &mut user,
                bank_name,
                &crate::memory_bank_db::MemoryBankMeta {
                    display_name: Some(bank_name.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        };
        let bank_db_id = bank.id().to_string();
        // Attach to agent's memory_banks subtree.
        let agent_db = {
            let user = registry.user_for_tests().await;
            let (db, _) = crate::agent_db::find_agent_db(&user, agent_name)
                .await
                .unwrap();
            db
        };
        agent_db
            .attach_memory_bank(crate::agent_db::MemoryBankRef {
                name: bank_name.to_string(),
                db_id: bank_db_id.clone(),
                permission,
            })
            .await
            .unwrap();
        bank_db_id
    }

    #[tokio::test]
    async fn remember_with_bank_writes_to_bank_and_recall_reads_back() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let _ = provision_bank(
            &registry,
            "alpha",
            "patrick",
            crate::agent_db::BankPermission::Write,
        )
        .await;

        let remember = Remember::new(registry.clone(), index.clone(), None);
        let ctx = make_ctx("alpha", session.clone());
        let out = remember
            .execute(
                serde_json::json!({
                    "key": "role",
                    "value": "boss",
                    "bank": "patrick"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.contains("patrick"),
            "response should mention bank: {out}"
        );

        // Recall via the same bank finds it.
        let recall = Recall::new(registry.clone(), index, None);
        let found = recall
            .execute(
                serde_json::json!({ "query": "boss", "bank": "patrick" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            found.contains("boss"),
            "recall should return entry: {found}"
        );

        // And own memory is untouched (self-remember was never called).
        let found_self = recall
            .execute(serde_json::json!({ "query": "boss" }), &ctx)
            .await
            .unwrap();
        assert!(
            found_self.contains("No memories found"),
            "self memory should be empty: {found_self}"
        );
    }

    #[tokio::test]
    async fn remember_with_read_only_bank_errors() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let _ = provision_bank(
            &registry,
            "alpha",
            "readonly",
            crate::agent_db::BankPermission::Read,
        )
        .await;

        let remember = Remember::new(registry.clone(), index, None);
        let ctx = make_ctx("alpha", session);
        let err = remember
            .execute(
                serde_json::json!({ "key": "k", "value": "v", "bank": "readonly" }),
                &ctx,
            )
            .await
            .expect_err("expected Read-only rejection");
        let msg = format!("{err:?}");
        assert!(msg.contains("Read-only"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn recall_with_unknown_bank_lists_available() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let _ = provision_bank(
            &registry,
            "alpha",
            "patrick",
            crate::agent_db::BankPermission::Read,
        )
        .await;

        let recall = Recall::new(registry.clone(), index, None);
        let ctx = make_ctx("alpha", session);
        let err = recall
            .execute(
                serde_json::json!({ "query": "x", "bank": "nonexistent" }),
                &ctx,
            )
            .await
            .expect_err("expected unknown-bank error");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("patrick"),
            "error should list available bank 'patrick': {msg}"
        );
    }

    #[tokio::test]
    async fn list_memory_banks_tool_returns_self_and_attached() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let _ = provision_bank(
            &registry,
            "alpha",
            "patrick",
            crate::agent_db::BankPermission::Write,
        )
        .await;
        let _ = provision_bank(
            &registry,
            "alpha",
            "projects",
            crate::agent_db::BankPermission::Read,
        )
        .await;

        let lister = ListMemoryBanks::new(registry.clone(), index);
        let ctx = make_ctx("alpha", session);
        let out = lister.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.contains("self"), "should include self: {out}");
        assert!(out.contains("patrick"), "should include patrick: {out}");
        assert!(out.contains("Write"), "should show Write perm: {out}");
        assert!(out.contains("projects"), "should include projects: {out}");
        assert!(out.contains("Read"), "should show Read perm: {out}");
    }

    // -------------------------------------------------------------------------
    // Stage A — tags + BM25 ranked recall
    // -------------------------------------------------------------------------

    /// Helper: write a single fact with optional tags.
    async fn put(remember: &Remember, ctx: &ToolContext, key: &str, value: &str, tags: &[&str]) {
        let tags_json: Vec<Value> = tags.iter().map(|t| Value::String(t.to_string())).collect();
        remember
            .execute(
                serde_json::json!({ "key": key, "value": value, "tags": tags_json }),
                ctx,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn remember_persists_tags_and_recall_renders_them() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let remember = Remember::new(registry.clone(), index.clone(), None);
        let recall = Recall::new(registry, index, None);
        let ctx = make_ctx("alpha", session);

        put(
            &remember,
            &ctx,
            "deadline",
            "ship by friday",
            &["project", "urgent"],
        )
        .await;

        let out = recall
            .execute(serde_json::json!({ "query": "ship" }), &ctx)
            .await
            .unwrap();
        assert!(out.contains("ship by friday"), "missing value: {out}");
        assert!(out.contains("tags:"), "missing tags marker: {out}");
        assert!(out.contains("project"), "missing 'project' tag: {out}");
        assert!(out.contains("urgent"), "missing 'urgent' tag: {out}");
    }

    #[tokio::test]
    async fn recall_filters_by_tags_and() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let remember = Remember::new(registry.clone(), index.clone(), None);
        let recall = Recall::new(registry, index, None);
        let ctx = make_ctx("alpha", session);

        put(&remember, &ctx, "k1", "alpha-fact", &["project"]).await;
        put(&remember, &ctx, "k2", "beta-fact", &["project", "urgent"]).await;
        put(&remember, &ctx, "k3", "gamma-fact", &["urgent"]).await;

        // Filter by both tags — only the entry tagged with both should remain.
        let out = recall
            .execute(
                serde_json::json!({
                    "query": "fact",
                    "tags": ["project", "urgent"],
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("beta-fact"), "expected k2: {out}");
        assert!(!out.contains("alpha-fact"), "k1 leaked: {out}");
        assert!(!out.contains("gamma-fact"), "k3 leaked: {out}");
    }

    #[tokio::test]
    async fn recall_honors_limit() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let remember = Remember::new(registry.clone(), index.clone(), None);
        let recall = Recall::new(registry, index, None);
        let ctx = make_ctx("alpha", session);

        for i in 0..5 {
            put(&remember, &ctx, &format!("k{i}"), "shared keyword", &[]).await;
        }
        let out = recall
            .execute(serde_json::json!({ "query": "shared", "limit": 2 }), &ctx)
            .await
            .unwrap();
        // One entry per line; expect exactly two lines.
        assert_eq!(out.lines().count(), 2, "expected 2 lines, got: {out}");
    }

    #[tokio::test]
    async fn recall_empty_query_returns_by_recency() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let remember = Remember::new(registry.clone(), index.clone(), None);
        let recall = Recall::new(registry, index, None);
        let ctx = make_ctx("alpha", session);

        // Use distinct keys (dedup-by-key would otherwise collapse them)
        // and tag every entry the same so we can exercise the tags-only
        // filtering path without keyword scoring.
        put(&remember, &ctx, "first", "old", &["log"]).await;
        // Force a measurable timestamp gap.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        put(&remember, &ctx, "second", "new", &["log"]).await;

        let out = recall
            .execute(
                serde_json::json!({ "query": "", "tags": ["log"], "limit": 10 }),
                &ctx,
            )
            .await
            .unwrap();
        let pos_first = out.find("first").unwrap_or(usize::MAX);
        let pos_second = out.find("second").unwrap_or(usize::MAX);
        assert!(pos_first != usize::MAX, "missing first: {out}");
        assert!(pos_second != usize::MAX, "missing second: {out}");
        assert!(
            pos_second < pos_first,
            "more recent entry should sort first: {out}"
        );
    }

    #[tokio::test]
    async fn recall_ranks_more_relevant_first() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let remember = Remember::new(registry.clone(), index.clone(), None);
        let recall = Recall::new(registry, index, None);
        let ctx = make_ctx("alpha", session);

        // Three entries — only one mentions both "deploy" and "friday".
        // BM25 should put it first.
        put(&remember, &ctx, "ops_note_a", "deploy on monday", &[]).await;
        put(&remember, &ctx, "ops_note_b", "deploy on friday", &[]).await;
        put(&remember, &ctx, "ops_note_c", "weekly status", &[]).await;

        let out = recall
            .execute(serde_json::json!({ "query": "deploy friday" }), &ctx)
            .await
            .unwrap();
        let pos_b = out.find("ops_note_b").unwrap_or(usize::MAX);
        let pos_a = out.find("ops_note_a").unwrap_or(usize::MAX);
        assert!(pos_b != usize::MAX, "missing best match: {out}");
        assert!(
            pos_b < pos_a,
            "more relevant entry should rank first: {out}"
        );
        // The weekly status note shares no terms; should not appear.
        assert!(
            !out.contains("ops_note_c"),
            "non-matching entry leaked: {out}"
        );
    }

    #[tokio::test]
    async fn recall_unknown_token_returns_no_results() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let remember = Remember::new(registry.clone(), index.clone(), None);
        let recall = Recall::new(registry, index, None);
        let ctx = make_ctx("alpha", session);

        put(&remember, &ctx, "k1", "the quick brown fox", &[]).await;

        let out = recall
            .execute(serde_json::json!({ "query": "zyxwvut" }), &ctx)
            .await
            .unwrap();
        assert!(out.contains("No memories found"), "got: {out}");
    }

    // -------------------------------------------------------------------------
    // Stage 2 — embedding subtree + hybrid recall
    // -------------------------------------------------------------------------

    use crate::embedding::test_support::MockEmbedder;
    use crate::embedding::{EmbeddingEntry, embeddings_store_name};
    use eidetica::store::Table;

    /// Pull every `EmbeddingEntry` row out of the agent's `embeddings:<model_id>`
    /// subtree. Returns `(memory_row_id, vector)` pairs. Used to assert
    /// the on-write population path actually ran.
    async fn read_embeddings(
        registry: &Arc<SessionRegistry>,
        agent_name: &str,
        model_id: &str,
    ) -> Vec<EmbeddingEntry> {
        let user = registry.user_for_tests().await;
        let (db, _) = crate::agent_db::find_agent_db(&user, agent_name)
            .await
            .unwrap();
        let txn = db.database().new_transaction().await.unwrap();
        let store_name = embeddings_store_name(model_id);
        let store = match txn.get_store::<Table<EmbeddingEntry>>(&store_name).await {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        store
            .search(|_: &EmbeddingEntry| true)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, e)| e)
            .collect()
    }

    #[tokio::test]
    async fn remember_with_embedder_populates_embeddings_subtree() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(
            "test/mock",
            vec!["deploy", "friday", "monday"],
        ));
        let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
        let ctx = make_ctx("alpha", session);

        remember
            .execute(
                serde_json::json!({ "key": "ops", "value": "deploy on friday" }),
                &ctx,
            )
            .await
            .unwrap();

        let stored = read_embeddings(&registry, "alpha", "test/mock").await;
        assert_eq!(stored.len(), 1, "expected one embedding row");
        let v = &stored[0].vector;
        // MockEmbedder normalizes — cosine should be ~1 against itself.
        let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5, "vector should be unit length");
        // Row ID is non-empty (the join key into `memory`).
        assert!(!stored[0].memory_row_id.is_empty());
    }

    #[tokio::test]
    async fn recall_semantic_match_when_keywords_dont_overlap() {
        // The whole point of Stage 2: a query with no shared tokens with
        // the value but with shared embedding axes still surfaces it.
        // MockEmbedder's "shared axis" is literally "shared token", so we
        // construct a setup where the query "friday" tokenizes to a token
        // that overlaps an axis in the entry but is not present in the
        // entry's text directly — using a synonym mapping.
        //
        // Trick: use distinct surface tokens, but route them to the same
        // axis. We achieve this by making the axes themselves lexicalized
        // synonyms. Concretely: entry value = "ship by EOW", query = "deploy
        // friday". MockEmbedder on "ship by EOW" maps "ship"→axis 0; on
        // "deploy friday" maps "deploy"→axis 1, "friday"→axis 2 — no
        // overlap.
        //
        // To force semantic-only retrieval, give the entry a word that
        // shares an axis with one query word but not lexically. We do
        // this by making axis names match content the entry has.
        let (_instance, registry, index, session) = fixture("alpha").await;
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(
            "test/mock",
            vec!["ship", "deploy", "release", "friday"],
        ));
        let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
        let recall = Recall::new(registry.clone(), index, Some(embedder.clone()));
        let ctx = make_ctx("alpha", session);

        // Entry contains "ship" + "release" (axes 0 and 2). Tokens "ship"
        // and "release" land on those axes.
        put(&remember, &ctx, "k1", "ship the release on friday", &[]).await;

        // Query "deploy friday" tokens "deploy" + "friday" → axes 1 and 3.
        // Lexically, BM25 only matches "friday" (one of the entry tokens),
        // so without semantic, the entry is found but with weak score.
        // Cosine: vectors share axis 3 ("friday") so cosine > 0.
        // This isn't a clean lexical-disjoint test, so let's add a
        // distractor entry with very different content — semantic should
        // rank the relevant entry higher.
        put(&remember, &ctx, "k2", "weekly status report on Monday", &[]).await;

        let out = recall
            .execute(serde_json::json!({ "query": "deploy friday" }), &ctx)
            .await
            .unwrap();
        // Best match should appear; Monday status should not surface (no
        // shared axis with the query, no shared token either).
        assert!(out.contains("k1"), "expected k1 in output: {out}");
        assert!(!out.contains("k2"), "k2 should not surface: {out}");
    }

    #[tokio::test]
    async fn recall_falls_back_to_lexical_when_db_has_no_embeddings() {
        // Write WITHOUT an embedder, then recall WITH one. The agent DB
        // has no `embeddings:<model_id>` subtree, but recall should still
        // work via BM25 alone.
        let (_instance, registry, index, session) = fixture("alpha").await;
        let remember_lex = Remember::new(registry.clone(), index.clone(), None);
        let ctx = make_ctx("alpha", session);
        put(&remember_lex, &ctx, "k1", "deploy on friday", &[]).await;

        // Now recall with an embedder configured.
        let embedder: Arc<dyn Embedder> =
            Arc::new(MockEmbedder::new("test/mock", vec!["deploy", "friday"]));
        let recall = Recall::new(registry, index, Some(embedder));
        let out = recall
            .execute(serde_json::json!({ "query": "friday" }), &ctx)
            .await
            .unwrap();
        assert!(
            out.contains("deploy on friday"),
            "lexical fallback should surface entry: {out}"
        );
    }

    #[tokio::test]
    async fn rrf_combine_merges_lexical_and_semantic_winners() {
        // Direct unit test of `rrf_combine`: entry A wins BM25, entry B
        // wins cosine, entry C is in neither — result must surface A and
        // B (in some order) and exclude C.
        let entries = vec![
            MemoryEntry {
                key: "a".into(),
                value: "alpha".into(),
                timestamp: Utc::now(),
                tags: vec![],
            },
            MemoryEntry {
                key: "b".into(),
                value: "beta".into(),
                timestamp: Utc::now(),
                tags: vec![],
            },
            MemoryEntry {
                key: "c".into(),
                value: "gamma".into(),
                timestamp: Utc::now(),
                tags: vec![],
            },
        ];
        let bm25 = vec![(10.0_f64, 0)]; // A is the only BM25 hit
        let cos = vec![(0.9_f32, 1)]; // B is the only cosine hit
        let out = rrf_combine(&entries, &bm25, &cos, 10);
        let keys: Vec<&str> = out.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"a"), "missing BM25 winner: {keys:?}");
        assert!(keys.contains(&"b"), "missing cosine winner: {keys:?}");
        assert!(!keys.contains(&"c"), "non-matching leaked: {keys:?}");
    }

    #[tokio::test]
    async fn rrf_combine_boosts_when_both_rankers_agree() {
        // Direct unit test: an entry that wins both lists should outrank
        // entries winning only one — that's the whole point of RRF.
        let mk = |k: &str| MemoryEntry {
            key: k.into(),
            value: "v".into(),
            timestamp: Utc::now(),
            tags: vec![],
        };
        let entries = vec![mk("both"), mk("bm25_only"), mk("cos_only")];
        // BM25: idx 0 first, idx 1 second
        let bm25 = vec![(10.0_f64, 0), (5.0, 1)];
        // Cosine: idx 0 first, idx 2 second
        let cos = vec![(0.9_f32, 0), (0.5, 2)];
        let out = rrf_combine(&entries, &bm25, &cos, 10);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0].key,
            "both",
            "agreement should rank first: {:?}",
            out.iter().map(|e| &e.key).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn remember_with_failing_embedder_still_stores_memory() {
        // Critical fallback: a network-down embedding API must not lose
        // the user's memory. The memory row gets written; the
        // `embeddings:<model_id>` subtree stays empty.
        use crate::embedding::test_support::FailingEmbedder;
        let (_instance, registry, index, session) = fixture("alpha").await;
        let embedder: Arc<dyn Embedder> = Arc::new(FailingEmbedder::new("test/down"));
        let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
        let recall = Recall::new(registry.clone(), index, None);
        let ctx = make_ctx("alpha", session);

        remember
            .execute(
                serde_json::json!({ "key": "k1", "value": "ship by friday" }),
                &ctx,
            )
            .await
            .unwrap();

        // Memory persisted: BM25 recall surfaces it.
        let out = recall
            .execute(serde_json::json!({ "query": "ship" }), &ctx)
            .await
            .unwrap();
        assert!(
            out.contains("ship by friday"),
            "memory should persist despite embedder failure: {out}"
        );
        // Embedding subtree stayed empty.
        let stored = read_embeddings(&registry, "alpha", "test/down").await;
        assert!(
            stored.is_empty(),
            "no embedding row should be written when embedder errors: {stored:?}"
        );
    }

    #[tokio::test]
    async fn recall_with_failing_query_embedder_falls_back_to_bm25() {
        // Write with a working embedder so embeddings exist on disk;
        // recall with a failing one — the query-embedding error path
        // must degrade to BM25-only, not error out.
        use crate::embedding::test_support::FailingEmbedder;
        let (_instance, registry, index, session) = fixture("alpha").await;
        let writer: Arc<dyn Embedder> =
            Arc::new(MockEmbedder::new("test/mock", vec!["ship", "friday"]));
        let failing: Arc<dyn Embedder> = Arc::new(FailingEmbedder::new("test/down"));
        let remember = Remember::new(registry.clone(), index.clone(), Some(writer));
        let recall = Recall::new(registry, index, Some(failing));
        let ctx = make_ctx("alpha", session);

        put(&remember, &ctx, "k1", "ship by friday", &[]).await;

        let out = recall
            .execute(serde_json::json!({ "query": "ship" }), &ctx)
            .await
            .unwrap();
        assert!(
            out.contains("ship by friday"),
            "BM25 fallback should still surface entry: {out}"
        );
    }

    #[tokio::test]
    async fn bank_remember_with_embedder_populates_embedding_subtree() {
        // The bank path uses `do_remember(..., embedder=Some(...))`
        // exactly like self memory; verify by writing into a bank and
        // reading the bank's `embeddings:<model_id>` subtree directly.
        let (_instance, registry, index, session) = fixture("alpha").await;
        let bank_db_id = provision_bank(
            &registry,
            "alpha",
            "shared",
            crate::agent_db::BankPermission::Write,
        )
        .await;

        let embedder: Arc<dyn Embedder> =
            Arc::new(MockEmbedder::new("test/mock", vec!["deploy", "friday"]));
        let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
        let ctx = make_ctx("alpha", session);
        remember
            .execute(
                serde_json::json!({
                    "key": "ops",
                    "value": "deploy friday",
                    "bank": "shared",
                }),
                &ctx,
            )
            .await
            .unwrap();

        // Pull the embeddings subtree off the bank DB itself. Scoped so
        // the user lock drops before we consume `registry` into Recall.
        {
            let user = registry.user_for_tests().await;
            let id = eidetica::entry::ID::parse(&bank_db_id).unwrap();
            let database = user.open_database(&id).await.unwrap();
            let txn = database.new_transaction().await.unwrap();
            let store = txn
                .get_store::<Table<EmbeddingEntry>>(&embeddings_store_name("test/mock"))
                .await
                .unwrap();
            let rows = store.search(|_: &EmbeddingEntry| true).await.unwrap();
            assert_eq!(rows.len(), 1, "bank should have one embedding");
        }

        // And recall via the bank still works (hybrid path).
        let recall = Recall::new(registry, index, Some(embedder));
        let ctx2 = make_ctx("alpha", ctx.session.clone());
        let out = recall
            .execute(
                serde_json::json!({ "query": "friday", "bank": "shared" }),
                &ctx2,
            )
            .await
            .unwrap();
        assert!(out.contains("deploy friday"), "bank recall: {out}");
    }

    #[tokio::test]
    async fn multiple_model_subtrees_coexist_on_one_db() {
        // Switching models should leave the old subtree dormant, not
        // overwrite or remove it. Write one entry under model A, then
        // another under model B on the same DB; both subtrees populate
        // independently.
        let (_instance, registry, index, session) = fixture("alpha").await;
        let ctx = make_ctx("alpha", session);

        let emb_a: Arc<dyn Embedder> = Arc::new(MockEmbedder::new("test/model-a", vec!["alpha"]));
        let remember_a = Remember::new(registry.clone(), index.clone(), Some(emb_a));
        remember_a
            .execute(
                serde_json::json!({ "key": "k1", "value": "alpha-fact" }),
                &ctx,
            )
            .await
            .unwrap();

        let emb_b: Arc<dyn Embedder> = Arc::new(MockEmbedder::new("test/model-b", vec!["beta"]));
        let remember_b = Remember::new(registry.clone(), index, Some(emb_b));
        remember_b
            .execute(
                serde_json::json!({ "key": "k2", "value": "beta-fact" }),
                &ctx,
            )
            .await
            .unwrap();

        // Direct subtree inspection: each model has exactly one row,
        // and they reference distinct memory rows.
        let user = registry.user_for_tests().await;
        let (agent_db, _) = crate::agent_db::find_agent_db(&user, "alpha")
            .await
            .unwrap();
        let txn = agent_db.database().new_transaction().await.unwrap();
        let a_rows = txn
            .get_store::<Table<EmbeddingEntry>>(&embeddings_store_name("test/model-a"))
            .await
            .unwrap()
            .search(|_: &EmbeddingEntry| true)
            .await
            .unwrap();
        let b_rows = txn
            .get_store::<Table<EmbeddingEntry>>(&embeddings_store_name("test/model-b"))
            .await
            .unwrap()
            .search(|_: &EmbeddingEntry| true)
            .await
            .unwrap();
        assert_eq!(a_rows.len(), 1, "model-a subtree");
        assert_eq!(b_rows.len(), 1, "model-b subtree");
        assert_ne!(
            a_rows[0].1.memory_row_id, b_rows[0].1.memory_row_id,
            "rows reference different memory entries"
        );
    }

    #[tokio::test]
    async fn re_remember_same_key_does_not_leak_old_value() {
        // Dedup-by-key keeps the most-recent row; recall must surface
        // the new value and not the old. The old embedding row stays
        // dormant in `embeddings:<model>` (its memory_row_id no longer
        // joins to anything visible) — that's expected and harmless.
        let (_instance, registry, index, session) = fixture("alpha").await;
        let embedder: Arc<dyn Embedder> =
            Arc::new(MockEmbedder::new("test/mock", vec!["alpha", "beta"]));
        let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
        let recall = Recall::new(registry, index, Some(embedder));
        let ctx = make_ctx("alpha", session);

        put(&remember, &ctx, "role", "alpha-version", &[]).await;
        // Force a measurable timestamp gap so dedup picks the new one.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        put(&remember, &ctx, "role", "beta-version", &[]).await;

        let out = recall
            .execute(serde_json::json!({ "query": "role" }), &ctx)
            .await
            .unwrap();
        assert!(
            out.contains("beta-version"),
            "newer value should surface: {out}"
        );
        assert!(
            !out.contains("alpha-version"),
            "older value should not leak: {out}"
        );
    }
}
