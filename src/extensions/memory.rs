//! Memory extension — `remember`, `recall`, `list_memory_banks` tools,
//! plus autonomous memory recall ("auto-recall") via [`ContextTail`] and a
//! [`MemoryAccess`] cap impl for other extensions.
//!
//! The extension manages both per-agent memory (AgentDb::memory) and
//! shared memory banks (MemoryBankDb::memory). All storage is eidetica.
//!
//! ## Configuration
//!
//! Per-agent auto-recall behaviour is stored in the agent DB `meta` store
//! under key `"memory_auto_recall"` as JSON. Manage it with:
//!
//! ```text
//! /memory config show
//! /memory config set auto_recall_enabled false
//! /memory config set max_entries 5
//! /memory config reset
//! ```

use crate::agent_db::{MEMORY_STORE, MemoryEntry};
use crate::embedding::Embedder;
use crate::extension::caps::{
    CapFuture, CapProvider, CapabilityKind, CapabilityRequest, CommandDescriptor, ContextTail,
    ExtensionCaps, MemoryAccess, MemoryHit, MemoryScope,
};
use crate::extension::handler::InstalledExtension;
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{
    Extension, ExtensionCommand, ExtensionCommandOutcome, ExtensionRef, HookContext, HookKind,
};
use crate::hosted_index::HostedIndex;
use crate::session::SessionRegistry;
use crate::tools::{ListMemoryBanks, Recall, Remember, search_memory, write_memory_entry};
use eidetica::Database;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ── Surfacing config ──────────────────────────────────────────────────

const AUTO_RECALL_CONFIG_KEY: &str = "memory_auto_recall";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AutoRecallConfig {
    /// Whether auto-recall runs at context assembly.
    #[serde(default = "default_true")]
    auto_recall_enabled: bool,
    /// Max entries to surface from own memory + each bank.
    #[serde(default = "default_max_entries")]
    max_entries: usize,
    /// Which banks participate in auto-recall. `None` = all attached banks.
    #[serde(default)]
    auto_recall_banks: Option<Vec<String>>,
}

fn default_true() -> bool {
    true
}
fn default_max_entries() -> usize {
    3
}
impl Default for AutoRecallConfig {
    fn default() -> Self {
        Self {
            auto_recall_enabled: true,
            max_entries: 3,
            auto_recall_banks: None,
        }
    }
}

async fn load_auto_recall_config(db: &Database) -> AutoRecallConfig {
    let txn = match db.new_transaction().await {
        Ok(t) => t,
        Err(_) => return AutoRecallConfig::default(),
    };
    let meta = match txn.get_store::<DocStore>("meta").await {
        Ok(s) => s,
        Err(_) => return AutoRecallConfig::default(),
    };
    match meta.get_string(AUTO_RECALL_CONFIG_KEY).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => AutoRecallConfig::default(),
    }
}

async fn save_auto_recall_config(db: &Database, config: &AutoRecallConfig) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>("meta").await?;
    let json = serde_json::to_string(config)?;
    store.set_string(AUTO_RECALL_CONFIG_KEY, &json).await?;
    txn.commit().await?;
    Ok(())
}

// ── Extension struct ──────────────────────────────────────────────────

pub struct MemoryExtension {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    embedder: Option<Arc<dyn Embedder>>,
}

impl MemoryExtension {
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

impl Extension for MemoryExtension {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool, HookKind::Command]
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![HookKind::Tool, HookKind::Command],
            required_capabilities: vec![
                CapabilityRequest::ToolRegistration,
                CapabilityRequest::CommandRegistration,
            ],
            requested_capabilities: Vec::new(),
            provides_capabilities: vec![CapabilityKind::Memory, CapabilityKind::ContextTail],
        }
    }

    fn build_providers(&self) -> anyhow::Result<HashMap<CapabilityKind, CapProvider>> {
        let mut map = HashMap::new();

        let ct: Arc<dyn ContextTail> = Arc::new(MemoryContextTail {
            registry: self.registry.clone(),
            agent_index: self.agent_index.clone(),
            embedder: self.embedder.clone(),
            session_attached_banks: Vec::new(),
        });
        map.insert(CapabilityKind::ContextTail, CapProvider::ContextTail(ct));

        let ma: Arc<dyn MemoryAccess> = Arc::new(MemoryAccessImpl {
            registry: self.registry.clone(),
            agent_index: self.agent_index.clone(),
            embedder: self.embedder.clone(),
        });
        map.insert(CapabilityKind::Memory, CapProvider::Memory(ma));

        Ok(map)
    }

    fn build_session_providers(
        &self,
        session_settings: &serde_json::Value,
    ) -> anyhow::Result<HashMap<CapabilityKind, CapProvider>> {
        let attached: Vec<String> = session_settings
            .get("attached_banks")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let ct: Arc<dyn ContextTail> = Arc::new(MemoryContextTail {
            registry: self.registry.clone(),
            agent_index: self.agent_index.clone(),
            embedder: self.embedder.clone(),
            session_attached_banks: attached,
        });

        let mut map = HashMap::new();
        map.insert(CapabilityKind::ContextTail, CapProvider::ContextTail(ct));
        Ok(map)
    }

    fn install<'a>(
        &'a self,
        caps: ExtensionCaps,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<InstalledExtension>> + Send + 'a>> {
        Box::pin(async move {
            let tool_reg = caps
                .tool_registration
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("memory install requires ToolRegistration cap"))?;
            let cmd_reg = caps.command_registration.as_ref().ok_or_else(|| {
                anyhow::anyhow!("memory install requires CommandRegistration cap")
            })?;

            let tools: Vec<Arc<dyn crate::tool::Tool>> = vec![
                Arc::new(Remember::new(
                    self.registry.clone(),
                    self.agent_index.clone(),
                    self.embedder.clone(),
                )),
                Arc::new(Recall::new(
                    self.registry.clone(),
                    self.agent_index.clone(),
                    self.embedder.clone(),
                )),
                Arc::new(ListMemoryBanks::new(
                    self.registry.clone(),
                    self.agent_index.clone(),
                )),
            ];
            for t in tools {
                let d = t.descriptor();
                tool_reg.register(d, t).await?;
            }

            cmd_reg
                .register(
                    CommandDescriptor {
                        name: "memory".into(),
                        description:
                            "Manage memory banks and auto-recall — attach | detach | config".into(),
                    },
                    Box::new(MemoryCommand {
                        registry: self.registry.clone(),
                        agent_index: self.agent_index.clone(),
                    }),
                )
                .await?;

            Ok(InstalledExtension::empty())
        })
    }
}

// ── MemoryContextTail ──────────────────────────────────────────────────

struct MemoryContextTail {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    embedder: Option<Arc<dyn Embedder>>,
    /// Per-session attached bank names (from extension_settings["memory"]["attached_banks"]).
    /// Populated by [`MemoryExtension::build_session_providers`].
    session_attached_banks: Vec<String>,
}

impl ContextTail for MemoryContextTail {
    fn context_tail<'a>(
        &'a self,
        agent_name: &'a str,
        recent_message_text: &'a [String],
    ) -> CapFuture<'a, Option<String>> {
        Box::pin(async move {
            let query = extract_query(recent_message_text);
            if query.is_empty() {
                return Ok(None);
            }

            let entry = match self.agent_index.find_by_name(agent_name) {
                Some(e) => e,
                None => return Ok(None),
            };

            let agent_db = match self
                .registry
                .open_agent_db(&entry.db_id, Some(&entry.pubkey))
                .await
            {
                Ok(Some(db)) => db,
                _ => return Ok(None),
            };

            let db = agent_db.database();

            // Read auto-recall config from agent DB
            let config = load_auto_recall_config(db).await;
            if !config.auto_recall_enabled {
                return Ok(None);
            }

            let max_entries = config.max_entries;

            // Search own memory
            let own_results = search_memory(
                db,
                MEMORY_STORE,
                &query,
                &[],
                max_entries,
                self.embedder.as_deref(),
            )
            .await
            .unwrap_or_default();

            // Collect bank names to search: persistent grants + session-scoped attachments
            let mut bank_names: Vec<String> = Vec::new();
            if let Ok(bank_refs) =
                crate::agent_db::read_blob::<Vec<crate::agent_db::MemoryBankRef>>(
                    db,
                    crate::agent_db::MEMORY_BANKS_STORE,
                )
                .await
            {
                for bref in &bank_refs {
                    if let Some(ref allowed) = config.auto_recall_banks
                        && !allowed.iter().any(|a| a == &bref.name)
                    {
                        continue;
                    }
                    bank_names.push(bref.name.clone());
                }
            }

            // Merge session-attached banks (populated by build_session_providers)
            for bank_name in &self.session_attached_banks {
                if !bank_names.contains(bank_name) {
                    bank_names.push(bank_name.clone());
                }
            }

            // Search each bank
            let mut bank_results = String::new();
            for bank_name in &bank_names {
                let Some(bank_entry) = self.agent_index.find_by_name(bank_name) else {
                    continue;
                };
                let bank = match self
                    .registry
                    .open_memory_bank(&bank_entry.db_id, Some(&bank_entry.pubkey))
                    .await
                {
                    Ok(Some(b)) => b,
                    _ => continue,
                };
                let results = search_memory(
                    bank.database(),
                    MEMORY_STORE,
                    &query,
                    &[],
                    max_entries,
                    self.embedder.as_deref(),
                )
                .await
                .unwrap_or_default();
                if !results.is_empty() {
                    bank_results.push_str(&format!("\n_(from bank: {})_\n{}", bank_name, results));
                }
            }

            if own_results.is_empty() && bank_results.is_empty() {
                return Ok(None);
            }

            let mut text = String::from("## Relevant Memories\n");
            text.push_str(&own_results);
            text.push_str(&bank_results);

            Ok(Some(text))
        })
    }
}

fn extract_query(recent_message_text: &[String]) -> String {
    let combined: String = recent_message_text
        .iter()
        .rev()
        .take(5)
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let tokens: Vec<&str> = combined
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2 && !is_stopword(t))
        .take(10)
        .collect();
    tokens.join(" ")
}

// ── MemoryAccessImpl ───────────────────────────────────────────────────

struct MemoryAccessImpl {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    embedder: Option<Arc<dyn Embedder>>,
}

impl MemoryAccess for MemoryAccessImpl {
    fn search<'a>(&'a self, query: &'a str, scope: MemoryScope) -> CapFuture<'a, Vec<MemoryHit>> {
        Box::pin(async move {
            let db = open_scope_db(&self.registry, &self.agent_index, &scope).await?;
            let formatted =
                search_memory(&db, MEMORY_STORE, query, &[], 10, self.embedder.as_deref())
                    .await
                    .unwrap_or_default();
            Ok(parse_memory_hits(&formatted))
        })
    }

    fn remember<'a>(
        &'a self,
        key: &'a str,
        value: &'a str,
        scope: MemoryScope,
    ) -> CapFuture<'a, ()> {
        Box::pin(async move {
            let db = open_scope_db(&self.registry, &self.agent_index, &scope).await?;
            let entry = MemoryEntry {
                key: key.to_string(),
                value: value.to_string(),
                timestamp: chrono::Utc::now(),
                tags: Vec::new(),
            };
            write_memory_entry(&db, MEMORY_STORE, entry, self.embedder.as_deref())
                .await
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            Ok(())
        })
    }
}

async fn open_scope_db(
    registry: &SessionRegistry,
    agent_index: &HostedIndex,
    scope: &MemoryScope,
) -> anyhow::Result<Database> {
    match scope {
        MemoryScope::Agent => Err(anyhow::anyhow!(
            "MemoryScope::Agent not yet supported via cap; use Bank scope"
        )),
        MemoryScope::Bank { name } => {
            let entry = agent_index
                .find_by_name(name)
                .ok_or_else(|| anyhow::anyhow!("memory bank not found: {}", name))?;
            let bank = registry
                .open_memory_bank(&entry.db_id, Some(&entry.pubkey))
                .await?
                .ok_or_else(|| anyhow::anyhow!("no key for memory bank: {}", name))?;
            Ok(bank.database().clone())
        }
    }
}

fn parse_memory_hits(formatted: &str) -> Vec<MemoryHit> {
    let mut hits = Vec::new();
    for line in formatted.lines() {
        if let Some(rest) = line.strip_prefix("- [")
            && let Some(close) = rest.find("]: ")
        {
            let key = rest[..close].to_string();
            let value = rest[close + 3..].to_string();
            hits.push(MemoryHit {
                key,
                value,
                score: 0.0,
                bank: None,
            });
        }
    }
    hits
}

// ── Slash command: /memory ────────────────────────────────────────────

struct MemoryCommand {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
}

impl ExtensionCommand for MemoryCommand {
    fn description(&self) -> &'static str {
        "Manage memory banks and auto-recall — attach | detach | config"
    }

    fn invoke<'a>(
        &'a self,
        args: &'a str,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
        Box::pin(async move {
            let args = args.trim();
            if let Some(bank_name) = args.strip_prefix("attach ") {
                attach_cmd(bank_name.trim(), ctx).await
            } else if let Some(bank_name) = args.strip_prefix("detach ") {
                detach_cmd(bank_name.trim(), ctx).await
            } else if args == "config" || args == "config show" {
                config_show_cmd(ctx, &self.registry, &self.agent_index).await
            } else if let Some(rest) = args.strip_prefix("config set ") {
                config_set_cmd(rest.trim(), ctx, &self.registry, &self.agent_index).await
            } else if args == "config reset" {
                config_reset_cmd(ctx, &self.registry, &self.agent_index).await
            } else {
                ExtensionCommandOutcome::Error(format!(
                    "Unknown memory sub-command: '{args}'. Use: attach <bank> | detach <bank> | config [show|set <key> <value>|reset]"
                ))
            }
        })
    }
}

async fn attach_cmd(bank_name: &str, ctx: &HookContext) -> ExtensionCommandOutcome {
    if bank_name.is_empty() {
        return ExtensionCommandOutcome::Error("Usage: /memory attach <bank_name>".into());
    }

    let mut settings = ctx.get_settings("memory").await;
    let banks = settings
        .as_object_mut()
        .and_then(|o| o.get_mut("attached_banks"))
        .and_then(|v| v.as_array_mut());

    let bank_json = serde_json::Value::String(bank_name.to_string());

    match banks {
        Some(arr) => {
            if arr.iter().any(|v| v == &bank_json) {
                return ExtensionCommandOutcome::Text(format!(
                    "Bank '{bank_name}' is already attached to this session."
                ));
            }
            arr.push(bank_json);
        }
        None => {
            settings = serde_json::json!({"attached_banks": [bank_name]});
        }
    }

    match ctx.set_settings("memory", settings).await {
        Ok(()) => ExtensionCommandOutcome::Text(format!(
            "Attached bank '{bank_name}' to this session. Its memories will be surfaced in context."
        )),
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to persist: {e}")),
    }
}

async fn detach_cmd(bank_name: &str, ctx: &HookContext) -> ExtensionCommandOutcome {
    if bank_name.is_empty() {
        return ExtensionCommandOutcome::Error("Usage: /memory detach <bank_name>".into());
    }

    let mut settings = ctx.get_settings("memory").await;
    let banks = settings
        .as_object_mut()
        .and_then(|o| o.get_mut("attached_banks"))
        .and_then(|v| v.as_array_mut());

    let banks = match banks {
        Some(a) => a,
        None => {
            return ExtensionCommandOutcome::Text(format!(
                "Bank '{bank_name}' is not attached to this session."
            ));
        }
    };

    let bank_json = serde_json::Value::String(bank_name.to_string());
    banks.retain(|v| v != &bank_json);

    match ctx.set_settings("memory", settings).await {
        Ok(()) => {
            ExtensionCommandOutcome::Text(format!("Detached bank '{bank_name}' from this session."))
        }
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to persist: {e}")),
    }
}

// ── Config sub-commands ────────────────────────────────────────────────

async fn config_show_cmd(
    ctx: &HookContext,
    registry: &SessionRegistry,
    agent_index: &HostedIndex,
) -> ExtensionCommandOutcome {
    let db = match open_agent_db_for_cmd(ctx, registry, agent_index).await {
        Ok(db) => db,
        Err(e) => return e,
    };
    let config = load_auto_recall_config(&db).await;
    let banks_str = match &config.auto_recall_banks {
        Some(b) if b.is_empty() => "(none)".to_string(),
        Some(b) => b.join(", "),
        None => "(all attached)".to_string(),
    };
    ExtensionCommandOutcome::Text(format!(
        "Auto-recall config:\n\
         ───────────────────────\n\
         auto_recall_enabled = {}\n\
         max_entries         = {}\n\
         auto_recall_banks   = {}\n\
         ───────────────────────\n\
         /memory config set <key> <value>  to change\n\
         /memory config reset              to revert to defaults",
        config.auto_recall_enabled, config.max_entries, banks_str,
    ))
}

async fn config_set_cmd(
    rest: &str,
    ctx: &HookContext,
    registry: &SessionRegistry,
    agent_index: &HostedIndex,
) -> ExtensionCommandOutcome {
    let (key, value) = match rest.split_once(' ') {
        Some((k, v)) => (k.trim(), v.trim()),
        None => {
            return ExtensionCommandOutcome::Error(
                "Usage: /memory config set <key> <value>\n\
                 Keys: auto_recall_enabled (true|false), max_entries (1-20), auto_recall_banks (comma-separated names)"
                    .into(),
            );
        }
    };

    let db = match open_agent_db_for_cmd(ctx, registry, agent_index).await {
        Ok(db) => db,
        Err(e) => return e,
    };
    let mut config = load_auto_recall_config(&db).await;

    match key {
        "auto_recall_enabled" => match value.to_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => config.auto_recall_enabled = true,
            "false" | "0" | "no" | "off" => config.auto_recall_enabled = false,
            _ => {
                return ExtensionCommandOutcome::Error(format!(
                    "Invalid value '{value}'. Use true or false."
                ));
            }
        },
        "max_entries" => match value.parse::<usize>() {
            Ok(n) if (1..=20).contains(&n) => config.max_entries = n,
            _ => {
                return ExtensionCommandOutcome::Error(format!(
                    "Invalid value '{value}'. Must be 1–20."
                ));
            }
        },
        "auto_recall_banks" => {
            let banks: Vec<String> = value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            config.auto_recall_banks = Some(banks);
        }
        _ => {
            return ExtensionCommandOutcome::Error(format!(
                "Unknown key '{key}'. Valid keys: auto_recall_enabled, max_entries, auto_recall_banks"
            ));
        }
    }

    match save_auto_recall_config(&db, &config).await {
        Ok(()) => ExtensionCommandOutcome::Text(format!(
            "Set {key} = {value}. Changes take effect next turn."
        )),
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to save config: {e}")),
    }
}

async fn config_reset_cmd(
    ctx: &HookContext,
    registry: &SessionRegistry,
    agent_index: &HostedIndex,
) -> ExtensionCommandOutcome {
    let db = match open_agent_db_for_cmd(ctx, registry, agent_index).await {
        Ok(db) => db,
        Err(e) => return e,
    };
    // Delete the key from meta — next load returns defaults
    let txn = match db.new_transaction().await {
        Ok(t) => t,
        Err(e) => return ExtensionCommandOutcome::Error(format!("Failed to open txn: {e}")),
    };
    let store = match txn.get_store::<DocStore>("meta").await {
        Ok(s) => s,
        Err(e) => return ExtensionCommandOutcome::Error(format!("Failed to open meta: {e}")),
    };
    match store.delete(AUTO_RECALL_CONFIG_KEY).await {
        Ok(_) => {
            let _ = txn.commit().await;
            ExtensionCommandOutcome::Text("Auto-recall config reset to defaults.".into())
        }
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to reset: {e}")),
    }
}

async fn open_agent_db_for_cmd(
    ctx: &HookContext,
    registry: &SessionRegistry,
    agent_index: &HostedIndex,
) -> Result<Database, ExtensionCommandOutcome> {
    let entry = agent_index.find_by_name(&ctx.agent_name).ok_or_else(|| {
        ExtensionCommandOutcome::Error(format!(
            "Agent '{}' has no Living Agent DB on this peer.",
            ctx.agent_name
        ))
    })?;
    let agent_db = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .map_err(|e| ExtensionCommandOutcome::Error(format!("Failed to open agent DB: {e}")))?
        .ok_or_else(|| {
            ExtensionCommandOutcome::Error(format!(
                "Peer holds no key for agent '{}' (DB {}).",
                ctx.agent_name, entry.db_id
            ))
        })?;
    Ok(agent_db.database().clone())
}

// ── Helpers ────────────────────────────────────────────────────────────

fn is_stopword(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "the"
            | "and"
            | "or"
            | "but"
            | "in"
            | "on"
            | "at"
            | "to"
            | "for"
            | "of"
            | "with"
            | "by"
            | "from"
            | "is"
            | "are"
            | "was"
            | "were"
            | "be"
            | "been"
            | "being"
            | "have"
            | "has"
            | "had"
            | "do"
            | "does"
            | "did"
            | "will"
            | "would"
            | "could"
            | "should"
            | "may"
            | "might"
            | "can"
            | "shall"
            | "i"
            | "me"
            | "my"
            | "we"
            | "us"
            | "our"
            | "you"
            | "your"
            | "he"
            | "she"
            | "it"
            | "its"
            | "they"
            | "them"
            | "their"
            | "this"
            | "that"
            | "these"
            | "those"
            | "not"
            | "no"
            | "if"
            | "then"
            | "else"
            | "when"
            | "where"
            | "how"
            | "what"
            | "which"
            | "who"
            | "whom"
            | "so"
            | "as"
            | "just"
            | "very"
            | "really"
            | "about"
            | "also"
            | "into"
            | "up"
            | "out"
    )
}
