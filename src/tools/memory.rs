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
use crate::hosted_index::HostedIndex;
use crate::session::SessionRegistry;
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolPolicy};
use chrono::Utc;
use eidetica::store::Table;
use eidetica::Database;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::debug;

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
        .open_agent_db(&entry.db_id)
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
    write_memory_entry(db, store, entry).await?;
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
) -> Result<String, String> {
    let query = str_arg(arguments, "query")?;
    let tags_filter = string_array_arg(arguments, "tags");
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_RECALL_LIMIT)
        .max(1);
    let result = search_memory(db, store, query, &tags_filter, limit).await?;
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
}

impl Remember {
    pub fn new(registry: Arc<SessionRegistry>, agent_index: HostedIndex) -> Self {
        Self {
            registry,
            agent_index,
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
            match arguments.get("bank").and_then(|v| v.as_str()) {
                None => do_remember(
                    ctx,
                    &arguments,
                    agent_db.database(),
                    crate::agent_db::MEMORY_STORE,
                    "Remembered",
                    "own",
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
}

impl Recall {
    pub fn new(registry: Arc<SessionRegistry>, agent_index: HostedIndex) -> Self {
        Self {
            registry,
            agent_index,
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
            match arguments.get("bank").and_then(|v| v.as_str()) {
                None => do_recall(
                    ctx,
                    &arguments,
                    agent_db.database(),
                    crate::agent_db::MEMORY_STORE,
                    "own",
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
        .open_memory_bank(&db_id)
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

/// Shared writer for both scopes.
async fn write_memory_entry(
    database: &Database,
    store_name: &str,
    entry: MemoryEntry,
) -> Result<(), String> {
    let txn = database
        .new_transaction()
        .await
        .map_err(|e| format!("Failed to create transaction: {e}"))?;
    let store = txn
        .get_store::<Table<MemoryEntry>>(store_name)
        .await
        .map_err(|e| format!("Failed to open memory store: {e}"))?;
    store
        .insert(entry)
        .await
        .map_err(|e| format!("Failed to store memory: {e}"))?;
    txn.commit()
        .await
        .map_err(|e| format!("Failed to commit memory: {e}"))?;
    Ok(())
}

/// Search memory entries by BM25 relevance, optionally pre-filtered by
/// tags, and return the top `limit` formatted as a Markdown list.
///
/// Pipeline:
/// 1. Load every entry from the store and dedupe by `key` (most recent
///    timestamp wins). The Table is append-only; the same `key` written
///    twice yields two rows, and the older one is logically stale.
/// 2. AND-filter by `tags_filter` (case-insensitive exact match per tag).
/// 3. If `query` tokenizes to nothing, return the surviving entries by
///    recency. If it does, BM25-score each entry's `key + value + tags`
///    document against the query, drop zero-score entries, sort, truncate.
async fn search_memory(
    database: &Database,
    store_name: &str,
    query: &str,
    tags_filter: &[String],
    limit: usize,
) -> Result<String, String> {
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

    let mut by_key: std::collections::HashMap<String, MemoryEntry> =
        std::collections::HashMap::new();
    for (_, entry) in records {
        by_key
            .entry(entry.key.clone())
            .and_modify(|existing| {
                if entry.timestamp > existing.timestamp {
                    *existing = entry.clone();
                }
            })
            .or_insert(entry);
    }
    let entries: Vec<MemoryEntry> = by_key
        .into_values()
        .filter(|e| entry_has_all_tags(e, tags_filter))
        .collect();

    if entries.is_empty() {
        return Ok(no_results_message(query, tags_filter));
    }

    let query_tokens = tokenize(query);
    let chosen: Vec<MemoryEntry> = if query_tokens.is_empty() {
        let mut sorted = entries;
        sorted.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        sorted.truncate(limit);
        sorted
    } else {
        rank_bm25(&entries, &query_tokens, limit)
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

/// Standard BM25 ranking. `k1=1.5`, `b=0.75` are textbook defaults; not
/// worth exposing as knobs at chaz scale (hundreds of entries per DB).
/// Each entry's "document" is `key + value + tags`. Returns up to
/// `limit` entries sorted by descending score; entries that don't match
/// any query term are dropped.
fn rank_bm25(entries: &[MemoryEntry], query_tokens: &[String], limit: usize) -> Vec<MemoryEntry> {
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
    scored.truncate(limit);
    scored
        .into_iter()
        .map(|(_, i)| entries[i].clone())
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
    use crate::agent_db::{create_agent_db, AgentDbConfig, AgentMeta};
    use crate::hosted_index::{DbEntry, HostedIndex};
    use crate::session::{Session, SessionRegistry};
    use crate::tool::{ScopedTools, ToolContext, ToolProfile, ToolRegistry};
    use crate::types::ConversationId;
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;
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
        }
    }

    #[tokio::test]
    async fn remember_writes_to_own_agent_db() {
        let (_instance, registry, index, session) = fixture("alpha").await;
        let tool = Remember::new(registry.clone(), index.clone());
        let ctx = make_ctx("alpha", session);

        tool.execute(
            serde_json::json!({ "key": "favorite_color", "value": "blue" }),
            &ctx,
        )
        .await
        .unwrap();

        let recall = Recall::new(registry, index);
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

        let remember = Remember::new(registry.clone(), index.clone());
        let recall = Recall::new(registry, index);

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

        let remember = Remember::new(registry.clone(), index.clone());
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
        let recall = Recall::new(registry.clone(), index);
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

        let remember = Remember::new(registry.clone(), index);
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

        let recall = Recall::new(registry.clone(), index);
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
        let remember = Remember::new(registry.clone(), index.clone());
        let recall = Recall::new(registry, index);
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
        let remember = Remember::new(registry.clone(), index.clone());
        let recall = Recall::new(registry, index);
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
        let remember = Remember::new(registry.clone(), index.clone());
        let recall = Recall::new(registry, index);
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
        let remember = Remember::new(registry.clone(), index.clone());
        let recall = Recall::new(registry, index);
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
        let remember = Remember::new(registry.clone(), index.clone());
        let recall = Recall::new(registry, index);
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
        let remember = Remember::new(registry.clone(), index.clone());
        let recall = Recall::new(registry, index);
        let ctx = make_ctx("alpha", session);

        put(&remember, &ctx, "k1", "the quick brown fox", &[]).await;

        let out = recall
            .execute(serde_json::json!({ "query": "zyxwvut" }), &ctx)
            .await
            .unwrap();
        assert!(out.contains("No memories found"), "got: {out}");
    }
}
