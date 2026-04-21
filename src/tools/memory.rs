//! Per-agent and peer-global memory tools.
//!
//! Stage 7 of Living Agents: moved memory storage into each Living Agent's
//! own `AgentDb::memory` store, with a new peer-global scope for cross-agent
//! sharing.
//!
//! - `remember` / `recall` → the running agent's own `AgentDb::memory`.
//! - `global_remember` / `global_recall` → the central DB's `global_memory`
//!   store, shared across all agents on this peer.
//!
//! Both scopes are capability-gated via `MemoryGrant`:
//! - `allow_self: true` (default) lets `remember`/`recall` open the agent's
//!   DB. Agents without the grant error out.
//! - `allow_global: false` (default) blocks `global_remember`/`global_recall`.
//!   Must be explicitly set in config (`tool_policies`) or per-agent overlay.

use crate::agent_db::MemoryEntry;
use crate::agent_index::AgentIndex;
use crate::grants::MemoryGrant;
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

/// Central-DB store name for peer-global memory. Not namespaced by agent —
/// any agent with `allow_global` shares this pool.
pub const GLOBAL_MEMORY_STORE: &str = "global_memory";

/// Shared helper: resolve the currently-running agent's `AgentDb` via the
/// index. Fails with a descriptive error if the agent has no DB on this
/// peer (e.g. imported without a key, or missing from the registry).
async fn open_own_agent_db(
    ctx: &ToolContext,
    registry: &SessionRegistry,
    index: &AgentIndex,
) -> Result<crate::agent_db::AgentDb, String> {
    let entry = index
        .find_by_name(&ctx.agent_name)
        .await
        .map_err(|e| format!("Agent index lookup failed: {e}"))?
        .ok_or_else(|| {
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

/// Check a specific MemoryGrant field. Returns a user-facing error if the
/// capability isn't granted.
fn require_memory_grant(
    ctx: &ToolContext,
    field: fn(&MemoryGrant) -> bool,
    scope_name: &str,
) -> Result<(), String> {
    let granted = ctx.grants().memory.as_ref().map(field).unwrap_or(false);
    if !granted {
        return Err(format!(
            "Memory capability '{scope_name}' not granted for agent '{}'. \
             Set grants.memory.allow_{scope_name} on this tool's policy or the agent's overlay.",
            ctx.agent_name
        ));
    }
    Ok(())
}

fn default_grant_self_only() -> crate::grants::Grants {
    crate::grants::Grants {
        memory: Some(MemoryGrant {
            allow_self: true,
            allow_global: false,
        }),
        ..Default::default()
    }
}

fn default_grant_global_off() -> crate::grants::Grants {
    crate::grants::Grants {
        memory: Some(MemoryGrant {
            allow_self: false,
            allow_global: false,
        }),
        ..Default::default()
    }
}

/// Extract a required string argument, returning a uniform error message.
fn str_arg<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing '{name}' argument"))
}

/// JSON Schema for the two write tools (`remember`, `global_remember`).
fn write_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "key":   { "type": "string", "description": "A short descriptive label for this fact (e.g. 'user_name', 'project_deadline')" },
            "value": { "type": "string", "description": "The fact to remember" }
        },
        "required": ["key", "value"]
    })
}

/// JSON Schema for the two read tools (`recall`, `global_recall`).
fn read_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Keyword to search for in memory keys and values" }
        },
        "required": ["query"]
    })
}

/// Parse `{key, value}`, write the entry to `(db, store)`, return the
/// success string. Shared by `Remember` and `GlobalRemember`.
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
    let entry = MemoryEntry {
        key: key.to_string(),
        value: value.to_string(),
        timestamp: Utc::now(),
    };
    write_memory_entry(db, store, entry).await?;
    debug!(agent = %ctx.agent_name, %key, scope = log_scope, "Stored memory");
    Ok(format!("{success_prefix}: {key} = {value}"))
}

/// Parse `{query}`, search `(db, store)`, return the formatted result.
/// Shared by `Recall` and `GlobalRecall`.
async fn do_recall(
    ctx: &ToolContext,
    arguments: &Value,
    db: &Database,
    store: &str,
    log_scope: &'static str,
) -> Result<String, String> {
    let query = str_arg(arguments, "query")?.to_lowercase();
    let result = search_memory(db, store, &query).await?;
    debug!(agent = %ctx.agent_name, %query, scope = log_scope, "Recalled memory");
    Ok(result)
}

/// Store a fact in the running agent's own persistent memory.
pub struct Remember {
    registry: Arc<SessionRegistry>,
    agent_index: AgentIndex,
}

impl Remember {
    pub fn new(registry: Arc<SessionRegistry>, agent_index: AgentIndex) -> Self {
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
            description: "Store a fact in this agent's own persistent memory. Saved to the agent's Living Agent DB so it syncs with the agent across peers.".to_string(),
            parameters: write_schema(),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            grants: default_grant_self_only(),
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            require_memory_grant(ctx, |g| g.allow_self, "self")?;
            let agent_db = open_own_agent_db(ctx, &self.registry, &self.agent_index).await?;
            do_remember(
                ctx,
                &arguments,
                agent_db.database(),
                crate::agent_db::MEMORY_STORE,
                "Remembered",
                "own",
            )
            .await
        })
    }
}

/// Search the running agent's own memory for facts.
pub struct Recall {
    registry: Arc<SessionRegistry>,
    agent_index: AgentIndex,
}

impl Recall {
    pub fn new(registry: Arc<SessionRegistry>, agent_index: AgentIndex) -> Self {
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
            description: "Search this agent's own persistent memory for previously stored facts."
                .to_string(),
            parameters: read_schema(),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            grants: default_grant_self_only(),
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            require_memory_grant(ctx, |g| g.allow_self, "self")?;
            let agent_db = open_own_agent_db(ctx, &self.registry, &self.agent_index).await?;
            do_recall(
                ctx,
                &arguments,
                agent_db.database(),
                crate::agent_db::MEMORY_STORE,
                "own",
            )
            .await
        })
    }
}

/// Store a fact in peer-global memory (shared across every agent on this peer).
pub struct GlobalRemember {
    central_db: Database,
}

impl GlobalRemember {
    pub fn new(central_db: Database) -> Self {
        Self { central_db }
    }
}

impl Tool for GlobalRemember {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "global_remember".to_string(),
            description: "Store a fact in peer-global memory, shared across every agent on this peer. Requires explicit grant — not available by default.".to_string(),
            parameters: write_schema(),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            grants: default_grant_global_off(),
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            require_memory_grant(ctx, |g| g.allow_global, "global")?;
            do_remember(
                ctx,
                &arguments,
                &self.central_db,
                GLOBAL_MEMORY_STORE,
                "Globally remembered",
                "global",
            )
            .await
        })
    }
}

/// Search peer-global memory for facts.
pub struct GlobalRecall {
    central_db: Database,
}

impl GlobalRecall {
    pub fn new(central_db: Database) -> Self {
        Self { central_db }
    }
}

impl Tool for GlobalRecall {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "global_recall".to_string(),
            description: "Search peer-global memory for facts shared across all agents on this peer. Requires explicit grant.".to_string(),
            parameters: read_schema(),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            grants: default_grant_global_off(),
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            require_memory_grant(ctx, |g| g.allow_global, "global")?;
            do_recall(
                ctx,
                &arguments,
                &self.central_db,
                GLOBAL_MEMORY_STORE,
                "global",
            )
            .await
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

/// Shared search for both scopes. Matches keys+values case-insensitively
/// and dedupes by key (keeping the most recent entry).
async fn search_memory(
    database: &Database,
    store_name: &str,
    query: &str,
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
        .search(|entry: &MemoryEntry| {
            entry.key.to_lowercase().contains(query) || entry.value.to_lowercase().contains(query)
        })
        .await
        .map_err(|e| format!("Failed to search memory: {e}"))?;

    if records.is_empty() {
        return Ok(format!("No memories found matching '{query}'."));
    }

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

    let result = by_key
        .values()
        .map(|m| {
            format!(
                "- **{}**: {} ({})",
                m.key,
                m.value,
                m.timestamp.to_rfc3339()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::agent_db::{create_agent_db, AgentDbConfig, AgentMeta};
    use crate::agent_index::{AgentIndex, AgentIndexEntry};
    use crate::grants::{Grants, MemoryGrant};
    use crate::session::{Session, SessionRegistry};
    use crate::tool::{ScopedTools, ToolContext, ToolProfile, ToolRegistry};
    use crate::types::ConversationId;
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    /// Full fixture: peer with a SessionRegistry + AgentIndex + one agent's
    /// DB registered, plus a dummy session so ToolContext has a valid handle.
    async fn fixture(
        agent_name: &str,
    ) -> (
        Instance,
        Arc<SessionRegistry>,
        AgentIndex,
        Arc<TokioMutex<Session>>,
        eidetica::Database, // central db for global tools
    ) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
        let agents_reg = Arc::new(AgentRegistry::from_config(&blank_config()));
        let registry = Arc::new(
            SessionRegistry::new(instance.clone(), user, agents_reg)
                .await
                .unwrap(),
        );
        let central_db = registry.central_db().clone();
        let index = AgentIndex::new(central_db.clone());

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
        index
            .register(AgentIndexEntry {
                db_id: agent_db.id(),
                display_name: agent_name.to_string(),
                pubkey,
            })
            .await
            .unwrap();

        // Need a session for ToolContext.session — just create a blank one.
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session = Arc::new(TokioMutex::new(
            Session::new(ConversationId(session_db.root_id().to_string()), session_db).await,
        ));

        (instance, registry, index, session, central_db)
    }

    fn blank_config() -> crate::config::Config {
        crate::config::Config {
            homeserver_url: String::new(),
            username: String::new(),
            password: None,
            allow_list: None,
            message_limit: None,
            room_size_limit: None,
            state_dir: None,
            chat_summary_model: None,
            role: None,
            roles: None,
            backends: None,
            agents: None,
            security: None,
            schedules: None,
            mcp_servers: None,
            tool_profiles: None,
            mcp_server_dir: None,
            context: None,
        }
    }

    fn make_ctx(
        agent_name: &str,
        session: Arc<TokioMutex<Session>>,
        grants: Grants,
    ) -> ToolContext {
        ToolContext {
            agent_name: agent_name.to_string(),
            call_depth: 0,
            max_call_depth: 10,
            tools: ScopedTools::new(Arc::new(ToolRegistry::new()), None),
            profile: ToolProfile::default(),
            session,
            grants,
            agent_grants: std::collections::HashMap::new(),
        }
    }

    fn self_grant() -> Grants {
        Grants {
            memory: Some(MemoryGrant {
                allow_self: true,
                allow_global: false,
            }),
            ..Default::default()
        }
    }

    fn global_grant() -> Grants {
        Grants {
            memory: Some(MemoryGrant {
                allow_self: false,
                allow_global: true,
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn remember_writes_to_own_agent_db() {
        let (_instance, registry, index, session, _central) = fixture("alpha").await;
        let tool = Remember::new(registry.clone(), index.clone());
        let ctx = make_ctx("alpha", session, self_grant());

        tool.execute(
            serde_json::json!({ "key": "favorite_color", "value": "blue" }),
            &ctx,
        )
        .await
        .unwrap();

        // Read back via Recall.
        let recall = Recall::new(registry, index);
        let ctx2 = make_ctx("alpha", ctx.session.clone(), self_grant());
        let result = recall
            .execute(serde_json::json!({ "query": "favorite" }), &ctx2)
            .await
            .unwrap();
        assert!(result.contains("blue"), "expected blue in {result}");
    }

    #[tokio::test]
    async fn remember_denied_without_grant() {
        let (_instance, registry, index, session, _central) = fixture("alpha").await;
        let tool = Remember::new(registry, index);
        // No memory grant at all.
        let ctx = make_ctx("alpha", session, Grants::default());

        let err = tool
            .execute(serde_json::json!({ "key": "x", "value": "y" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("not granted"), "unexpected: {err}");
    }

    #[tokio::test]
    async fn remember_denied_when_allow_self_is_false() {
        let (_instance, registry, index, session, _central) = fixture("alpha").await;
        let tool = Remember::new(registry, index);
        let explicit_no = Grants {
            memory: Some(MemoryGrant {
                allow_self: false,
                allow_global: true,
            }),
            ..Default::default()
        };
        let ctx = make_ctx("alpha", session, explicit_no);

        let err = tool
            .execute(serde_json::json!({ "key": "x", "value": "y" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("not granted"));
    }

    #[tokio::test]
    async fn per_agent_memory_is_isolated() {
        // alpha and beta are separate agents on the same peer. Writing under
        // alpha must not appear under beta's recall.
        let (_instance, registry, index, session, _central) = fixture("alpha").await;
        // Register a second agent.
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
        index
            .register(AgentIndexEntry {
                db_id: beta_db.id(),
                display_name: "beta".to_string(),
                pubkey: beta_pubkey,
            })
            .await
            .unwrap();

        let remember = Remember::new(registry.clone(), index.clone());
        let recall = Recall::new(registry, index);

        // alpha writes.
        let ctx_alpha = make_ctx("alpha", session.clone(), self_grant());
        remember
            .execute(
                serde_json::json!({ "key": "secret", "value": "alpha-only" }),
                &ctx_alpha,
            )
            .await
            .unwrap();

        // beta recalls — should NOT see alpha's value.
        let ctx_beta = make_ctx("beta", session, self_grant());
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

    #[tokio::test]
    async fn global_remember_requires_grant() {
        let (_instance, _registry, _index, session, central) = fixture("alpha").await;
        let tool = GlobalRemember::new(central);
        // Self-only grant should NOT authorize global.
        let ctx = make_ctx("alpha", session, self_grant());

        let err = tool
            .execute(serde_json::json!({ "key": "x", "value": "y" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("'global'"), "unexpected: {err}");
    }

    #[tokio::test]
    async fn global_scope_writes_to_central_and_is_shared() {
        // alpha writes to global; beta reads from global and sees it.
        let (_instance, _registry, _index, session, central) = fixture("alpha").await;

        let remember = GlobalRemember::new(central.clone());
        let recall = GlobalRecall::new(central);

        let ctx_alpha = make_ctx("alpha", session.clone(), global_grant());
        remember
            .execute(
                serde_json::json!({ "key": "company", "value": "arcuru" }),
                &ctx_alpha,
            )
            .await
            .unwrap();

        let ctx_beta = make_ctx("beta", session, global_grant());
        let result = recall
            .execute(serde_json::json!({ "query": "company" }), &ctx_beta)
            .await
            .unwrap();
        assert!(
            result.contains("arcuru"),
            "expected cross-agent visibility: {result}"
        );
    }
}
