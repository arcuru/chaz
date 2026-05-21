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
use crate::tools::{
    ListMemoryBanks, Recall, Remember, search_memory, search_memory_structured, write_memory_entry,
};
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
    memory_bank_index: HostedIndex,
    embedder: Option<Arc<dyn Embedder>>,
}

impl MemoryExtension {
    pub fn new(
        registry: Arc<SessionRegistry>,
        agent_index: HostedIndex,
        memory_bank_index: HostedIndex,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Self {
        Self {
            registry,
            agent_index,
            memory_bank_index,
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
            memory_bank_index: self.memory_bank_index.clone(),
            embedder: self.embedder.clone(),
            session_attached_banks: Vec::new(),
        });
        map.insert(CapabilityKind::ContextTail, CapProvider::ContextTail(ct));

        let ma: Arc<dyn MemoryAccess> = Arc::new(MemoryAccessImpl {
            registry: self.registry.clone(),
            agent_index: self.agent_index.clone(),
            memory_bank_index: self.memory_bank_index.clone(),
            embedder: self.embedder.clone(),
        });
        map.insert(CapabilityKind::Memory, CapProvider::Memory(ma));

        Ok(map)
    }

    // ── Lifecycle: per-session ────────────────────────────────────────
    //
    // Memory is the first extension migrated to the per-session
    // lifecycle. The hub instantiates one [`MemoryInstance`] per
    // session on first dispatch, the instance holds the resolved
    // `attached_banks` list, and its `context_tail()` endpoint is what
    // the auto-recall path consults. The legacy `build_session_providers`
    // path (which rebuilt the provider every turn) is gone — the
    // per-turn re-read of session settings was the band-aid the
    // lifecycle work is here to replace.

    fn scope(&self) -> crate::extension::Scope {
        crate::extension::Scope::PerSession
    }

    fn instantiate<'a>(
        &'a self,
        scope_ctx: crate::extension::ScopeCtx<'a>,
    ) -> crate::extension::instance::InstantiateFuture<'a> {
        let registry = self.registry.clone();
        let agent_index = self.agent_index.clone();
        let memory_bank_index = self.memory_bank_index.clone();
        let embedder = self.embedder.clone();
        let manifest = self.manifest();
        Box::pin(async move {
            let attached: Vec<String> = match &scope_ctx {
                crate::extension::ScopeCtx::Session { session_db, .. } => {
                    let settings = crate::extension::read_settings(session_db, "memory").await;
                    settings
                        .get("attached_banks")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default()
                }
                _ => Vec::new(),
            };

            let context_tail: Arc<dyn ContextTail> = Arc::new(MemoryContextTail {
                registry,
                agent_index,
                memory_bank_index,
                embedder,
                session_attached_banks: attached,
            });

            Ok(Arc::new(MemoryInstance {
                manifest,
                context_tail,
            })
                as Arc<dyn crate::extension::ExtensionInstance>)
        })
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
                        description: "Manage memory banks: list | new | delete | grant | revoke | \
                                      share | unshare | import | attach | detach | config"
                            .into(),
                    },
                    Box::new(MemoryCommand {
                        registry: self.registry.clone(),
                        agent_index: self.agent_index.clone(),
                        memory_bank_index: self.memory_bank_index.clone(),
                    }),
                )
                .await?;

            Ok(InstalledExtension::empty())
        })
    }
}

// ── Per-session instance ─────────────────────────────────────────────
//
// The runtime view of memory for one session. Holds the resolved
// `MemoryContextTail` (with attached_banks captured at instantiation
// time) and publishes it via the `context_tail()` endpoint the hub
// invokes on per-turn dispatch.

struct MemoryInstance {
    manifest: ExtensionManifest,
    context_tail: Arc<dyn ContextTail>,
}

impl crate::extension::ExtensionInstance for MemoryInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn context_tail(&self) -> Option<Arc<dyn ContextTail>> {
        Some(self.context_tail.clone())
    }
}

// ── MemoryContextTail ──────────────────────────────────────────────────

struct MemoryContextTail {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    memory_bank_index: HostedIndex,
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
                let Some(bank_entry) = self.memory_bank_index.find_by_name(bank_name) else {
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
    memory_bank_index: HostedIndex,
    embedder: Option<Arc<dyn Embedder>>,
}

impl MemoryAccess for MemoryAccessImpl {
    fn search<'a>(
        &'a self,
        agent_name: &'a str,
        query: &'a str,
        scope: MemoryScope,
    ) -> CapFuture<'a, Vec<MemoryHit>> {
        Box::pin(async move {
            let (db, bank_label) = open_scope_db(
                &self.registry,
                &self.agent_index,
                &self.memory_bank_index,
                agent_name,
                &scope,
            )
            .await?;
            let scored = search_memory_structured(
                &db,
                MEMORY_STORE,
                query,
                &[],
                10,
                self.embedder.as_deref(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
            Ok(scored
                .into_iter()
                .map(|h| MemoryHit {
                    key: h.entry.key,
                    value: h.entry.value,
                    score: h.score,
                    bank: bank_label.clone(),
                })
                .collect())
        })
    }

    fn remember<'a>(
        &'a self,
        agent_name: &'a str,
        key: &'a str,
        value: &'a str,
        scope: MemoryScope,
    ) -> CapFuture<'a, ()> {
        Box::pin(async move {
            let (db, _) = open_scope_db(
                &self.registry,
                &self.agent_index,
                &self.memory_bank_index,
                agent_name,
                &scope,
            )
            .await?;
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

/// Resolve a [`MemoryScope`] to its backing eidetica `Database` and a
/// human-readable bank label (`None` for the agent's own memory). The
/// label propagates onto each [`MemoryHit::bank`] so downstream
/// consumers can attribute hits without re-walking the scope.
async fn open_scope_db(
    registry: &SessionRegistry,
    agent_index: &HostedIndex,
    memory_bank_index: &HostedIndex,
    agent_name: &str,
    scope: &MemoryScope,
) -> anyhow::Result<(Database, Option<String>)> {
    match scope {
        MemoryScope::Agent => {
            let entry = agent_index
                .find_by_name(agent_name)
                .ok_or_else(|| anyhow::anyhow!("agent not found: {}", agent_name))?;
            let agent_db = registry
                .open_agent_db(&entry.db_id, Some(&entry.pubkey))
                .await?
                .ok_or_else(|| anyhow::anyhow!("no key for agent: {}", agent_name))?;
            Ok((agent_db.database().clone(), None))
        }
        MemoryScope::Bank { name } => {
            let entry = memory_bank_index
                .find_by_name(name)
                .ok_or_else(|| anyhow::anyhow!("memory bank not found: {}", name))?;
            let bank = registry
                .open_memory_bank(&entry.db_id, Some(&entry.pubkey))
                .await?
                .ok_or_else(|| anyhow::anyhow!("no key for memory bank: {}", name))?;
            Ok((bank.database().clone(), Some(name.clone())))
        }
    }
}

// ── Slash command: /memory ────────────────────────────────────────────

struct MemoryCommand {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    memory_bank_index: HostedIndex,
}

impl ExtensionCommand for MemoryCommand {
    fn description(&self) -> &'static str {
        "Manage memory banks: list | new | delete | grant | revoke | share | unshare | import | \
         attach <name|db_id|ticket> | detach | config"
    }

    fn invoke<'a>(
        &'a self,
        args: &'a str,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
        Box::pin(async move {
            let args = args.trim();
            // Bank CRUD — read-only first, then mutating, then sharing.
            if args.is_empty() || args == "list" {
                return self.list_cmd().await;
            }
            if let Some(rest) = args.strip_prefix("new ") {
                return self.new_cmd(rest.trim()).await;
            }
            if let Some(rest) = args
                .strip_prefix("delete ")
                .or_else(|| args.strip_prefix("del "))
            {
                return self.delete_cmd(rest.trim()).await;
            }
            if let Some(rest) = args.strip_prefix("grant ") {
                return self.grant_cmd(rest.trim()).await;
            }
            if let Some(rest) = args.strip_prefix("revoke ") {
                return self.revoke_cmd(rest.trim()).await;
            }
            if let Some(rest) = args.strip_prefix("share ") {
                return self.share_cmd(rest.trim()).await;
            }
            if let Some(rest) = args.strip_prefix("unshare ") {
                return self.unshare_cmd(rest.trim()).await;
            }
            if let Some(rest) = args.strip_prefix("import ") {
                return self.import_cmd(rest.trim()).await;
            }
            // Per-session attachments and auto-recall config.
            if let Some(arg) = args.strip_prefix("attach ") {
                return self.attach_cmd(arg.trim(), ctx).await;
            }
            if let Some(bank_name) = args.strip_prefix("detach ") {
                return detach_cmd(bank_name.trim(), ctx).await;
            }
            if args == "config" || args == "config show" {
                return config_show_cmd(ctx, &self.registry, &self.agent_index).await;
            }
            if let Some(rest) = args.strip_prefix("config set ") {
                return config_set_cmd(rest.trim(), ctx, &self.registry, &self.agent_index).await;
            }
            if args == "config reset" {
                return config_reset_cmd(ctx, &self.registry, &self.agent_index).await;
            }
            ExtensionCommandOutcome::Error(format!(
                "Unknown memory sub-command: '{args}'. Use: list | new <name> [desc] | \
                 delete <bank> | grant <bank> <agent> <read|write> | revoke <bank> <agent> | \
                 share <bank> | unshare <bank> | import <ticket> [admin|write|read] | \
                 attach <bank|db_id|ticket> | detach <bank> | \
                 config [show|set <key> <value>|reset]"
            ))
        })
    }
}

// ── Bank CRUD — list/new/delete/grant/revoke/share/unshare/import ─────
//
// These were built-in `/memory` subcommands until memory became a
// first-class extension. They now live alongside the per-session
// attach/detach and auto-recall config so the entire `/memory` surface
// flows through one extension command.

impl MemoryCommand {
    fn resolve_bank(&self, bank_ref: &str) -> Result<crate::hosted_index::DbEntry, String> {
        if let Some(entry) = self.memory_bank_index.find_by_name(bank_ref) {
            return Ok(entry);
        }
        if let Ok(id) = eidetica::entry::ID::parse(bank_ref)
            && let Some(entry) = self.memory_bank_index.find_by_id(&id)
        {
            return Ok(entry);
        }
        Err(format!(
            "No hosted memory bank matches '{bank_ref}' (try a display name from /memory list \
             or a bank DB ID)"
        ))
    }

    fn resolve_agent(&self, agent_ref: &str) -> Result<crate::hosted_index::DbEntry, String> {
        if let Some(entry) = self.agent_index.find_by_name(agent_ref) {
            return Ok(entry);
        }
        if let Ok(id) = eidetica::entry::ID::parse(agent_ref)
            && let Some(entry) = self.agent_index.find_by_id(&id)
        {
            return Ok(entry);
        }
        Err(format!(
            "No hosted agent matches '{agent_ref}' (try a display name from /agents or an agent \
             DB ID)"
        ))
    }

    async fn list_cmd(&self) -> ExtensionCommandOutcome {
        let entries = self.memory_bank_index.list();
        if entries.is_empty() {
            return ExtensionCommandOutcome::Text(
                "No memory banks on this peer. Create one with /memory new <name>.".into(),
            );
        }
        let lines: Vec<String> = entries
            .iter()
            .map(|e| format!("  {} ({})", e.display_name, e.db_id))
            .collect();
        ExtensionCommandOutcome::Text(format!("Memory banks on this peer:\n{}", lines.join("\n")))
    }

    async fn new_cmd(&self, rest: &str) -> ExtensionCommandOutcome {
        let (name, desc) = match rest.split_once(char::is_whitespace) {
            Some((n, d)) => (n.trim(), Some(d.trim().to_string())),
            None => (rest, None),
        };
        let desc = desc.filter(|s| !s.is_empty());
        if name.is_empty() {
            return ExtensionCommandOutcome::Error("Memory bank name required".into());
        }

        let meta = crate::memory_bank_db::MemoryBankMeta {
            display_name: Some(name.to_string()),
            description: desc,
        };

        let (bank, pubkey) = match self.registry.create_new_memory_bank(name, &meta).await {
            Ok(p) => p,
            Err(e) => {
                return ExtensionCommandOutcome::Error(format!(
                    "Failed to create memory bank: {e}"
                ));
            }
        };

        self.memory_bank_index
            .register(crate::hosted_index::DbEntry {
                db_id: bank.id(),
                display_name: name.to_string(),
                pubkey,
            });

        ExtensionCommandOutcome::Text(format!(
            "Created memory bank '{name}' (DB {}). Grant it to an agent with /memory grant.",
            bank.id()
        ))
    }

    async fn delete_cmd(&self, bank_ref: &str) -> ExtensionCommandOutcome {
        if bank_ref.is_empty() {
            return ExtensionCommandOutcome::Error("Usage: /memory delete <name|db_id>".into());
        }
        let entry = match self.resolve_bank(bank_ref) {
            Ok(e) => e,
            Err(msg) => return ExtensionCommandOutcome::Error(msg),
        };

        self.memory_bank_index.unregister(&entry.db_id);

        ExtensionCommandOutcome::Text(format!(
            "Deleted memory bank '{}' (DB {} preserved for archive). Agents with this bank in \
             their memory_banks subtree will still see it listed — use /memory revoke to remove \
             grants.",
            entry.display_name, entry.db_id
        ))
    }

    /// Order matters: auth (authoritative) → ref mirror. If the mirror
    /// fails, best-effort revoke the auth so the two stores stay consistent.
    async fn grant_cmd(&self, rest: &str) -> ExtensionCommandOutcome {
        let mut parts = rest.splitn(3, char::is_whitespace);
        let bank_ref = parts.next().unwrap_or("").trim();
        let agent_ref = parts.next().unwrap_or("").trim();
        let perm_tok = parts.next().unwrap_or("").trim();
        if bank_ref.is_empty() || agent_ref.is_empty() || perm_tok.is_empty() {
            return ExtensionCommandOutcome::Error(
                "Usage: /memory grant <bank> <agent> <read|write>".into(),
            );
        }
        let permission = match perm_tok.to_ascii_lowercase().as_str() {
            "read" | "r" => crate::agent_db::BankPermission::Read,
            "write" | "w" => crate::agent_db::BankPermission::Write,
            _ => {
                return ExtensionCommandOutcome::Error(format!(
                    "Unknown permission '{perm_tok}' — use read or write"
                ));
            }
        };

        let bank = match self.resolve_bank(bank_ref) {
            Ok(e) => e,
            Err(msg) => return ExtensionCommandOutcome::Error(msg),
        };
        let agent = match self.resolve_agent(agent_ref) {
            Ok(e) => e,
            Err(msg) => return ExtensionCommandOutcome::Error(msg),
        };

        let key_label = format!("memory:{}:{}", bank.display_name, agent.display_name);
        if let Err(e) = self
            .registry
            .grant_on_memory_bank(&bank.db_id, &agent.pubkey, &key_label, permission)
            .await
        {
            return ExtensionCommandOutcome::Error(format!(
                "Failed to authorize agent on bank: {e}"
            ));
        }

        let agent_db = match self
            .registry
            .open_agent_db(&agent.db_id, Some(&agent.pubkey))
            .await
        {
            Ok(Some(db)) => db,
            Ok(None) => {
                let _ = self
                    .registry
                    .revoke_on_memory_bank(&bank.db_id, &agent.pubkey)
                    .await;
                return ExtensionCommandOutcome::Error(format!(
                    "Granted auth but can't open agent '{}'s DB to record the ref — rolled back",
                    agent.display_name
                ));
            }
            Err(e) => {
                let _ = self
                    .registry
                    .revoke_on_memory_bank(&bank.db_id, &agent.pubkey)
                    .await;
                return ExtensionCommandOutcome::Error(format!(
                    "Granted auth but failed to open agent DB — rolled back: {e}"
                ));
            }
        };

        let ref_entry = crate::agent_db::MemoryBankRef {
            name: bank.display_name.clone(),
            db_id: bank.db_id.to_string(),
            permission,
        };
        if let Err(e) = agent_db.attach_memory_bank(ref_entry).await {
            let _ = self
                .registry
                .revoke_on_memory_bank(&bank.db_id, &agent.pubkey)
                .await;
            return ExtensionCommandOutcome::Error(format!(
                "Granted auth but failed to write ref to agent DB — rolled back: {e}"
            ));
        }

        ExtensionCommandOutcome::Text(format!(
            "Granted agent '{}' {:?} access to memory bank '{}'",
            agent.display_name, permission, bank.display_name
        ))
    }

    async fn revoke_cmd(&self, rest: &str) -> ExtensionCommandOutcome {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let bank_ref = parts.next().unwrap_or("").trim();
        let agent_ref = parts.next().unwrap_or("").trim();
        if bank_ref.is_empty() || agent_ref.is_empty() {
            return ExtensionCommandOutcome::Error("Usage: /memory revoke <bank> <agent>".into());
        }
        let bank = match self.resolve_bank(bank_ref) {
            Ok(e) => e,
            Err(msg) => return ExtensionCommandOutcome::Error(msg),
        };
        let agent = match self.resolve_agent(agent_ref) {
            Ok(e) => e,
            Err(msg) => return ExtensionCommandOutcome::Error(msg),
        };

        if let Err(e) = self
            .registry
            .revoke_on_memory_bank(&bank.db_id, &agent.pubkey)
            .await
        {
            return ExtensionCommandOutcome::Error(format!("Failed to revoke auth: {e}"));
        }

        let ref_removed = match self
            .registry
            .open_agent_db(&agent.db_id, Some(&agent.pubkey))
            .await
        {
            Ok(Some(db)) => db.detach_memory_bank(&bank.display_name).await.ok(),
            _ => None,
        };

        let mut msg = format!(
            "Revoked agent '{}'s access to memory bank '{}'",
            agent.display_name, bank.display_name
        );
        if ref_removed != Some(true) {
            msg.push_str(
                " (note: couldn't remove the ref from the agent's memory_banks subtree — auth \
                 is revoked regardless)",
            );
        }
        ExtensionCommandOutcome::Text(msg)
    }

    async fn share_cmd(&self, bank_ref: &str) -> ExtensionCommandOutcome {
        if bank_ref.is_empty() {
            return ExtensionCommandOutcome::Error("Usage: /memory share <bank>".into());
        }
        let entry = match self.resolve_bank(bank_ref) {
            Ok(e) => e,
            Err(msg) => return ExtensionCommandOutcome::Error(msg),
        };
        let instance = self.registry.instance();
        if instance.sync().is_none() {
            return ExtensionCommandOutcome::Error("Sync not enabled".into());
        }
        match self.registry.share_for(&entry.db_id).await {
            Ok(ticket) => ExtensionCommandOutcome::Text(format!(
                "Share this ticket to sync memory bank '{}' (DB {}):\n\n{ticket}",
                entry.display_name, entry.db_id
            )),
            Err(e) => ExtensionCommandOutcome::Error(format!("Failed to share memory bank: {e}")),
        }
    }

    async fn unshare_cmd(&self, bank_ref: &str) -> ExtensionCommandOutcome {
        if bank_ref.is_empty() {
            return ExtensionCommandOutcome::Error("Usage: /memory unshare <bank>".into());
        }
        let entry = match self.resolve_bank(bank_ref) {
            Ok(e) => e,
            Err(msg) => return ExtensionCommandOutcome::Error(msg),
        };
        match self.registry.disable_sync_for(&entry.db_id).await {
            Ok(()) => ExtensionCommandOutcome::Text(format!(
                "Sync disabled for memory bank '{}' — it is no longer shared.",
                entry.display_name
            )),
            Err(e) => ExtensionCommandOutcome::Error(format!("Failed to disable sync: {e}")),
        }
    }

    async fn import_cmd(&self, rest: &str) -> ExtensionCommandOutcome {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let ticket_str = parts.next().unwrap_or("").trim();
        let perm_tok = parts.next().unwrap_or("").trim();
        if ticket_str.is_empty() {
            return ExtensionCommandOutcome::Error(
                "Usage: /memory import <ticket> [admin|write|read]".into(),
            );
        }
        let permission = match perm_tok {
            "" => crate::commands::CoOwnerPermission::Write,
            other => match crate::commands::parse_permission_token(other) {
                Some(p) => p,
                None => {
                    return ExtensionCommandOutcome::Error(format!(
                        "Unknown permission '{other}' — use admin, write, or read (default: write)"
                    ));
                }
            },
        };
        let ticket: eidetica::sync::DatabaseTicket = match ticket_str.parse() {
            Ok(t) => t,
            Err(e) => return ExtensionCommandOutcome::Error(format!("Invalid ticket: {e}")),
        };
        match self.import_bank_via_ticket(&ticket, permission).await {
            Ok(ImportOutcome::Imported {
                display_name,
                db_id,
            }) => ExtensionCommandOutcome::Text(format!(
                "Imported memory bank '{display_name}' (DB {db_id}). Grant it to agents with \
                 /memory grant {display_name} <agent> <read|write>."
            )),
            Ok(ImportOutcome::AlreadyLocal { display_name }) => ExtensionCommandOutcome::Text(
                format!("Memory bank '{display_name}' is already hosted on this peer."),
            ),
            Ok(ImportOutcome::Pending { request_id }) => ExtensionCommandOutcome::Text(format!(
                "Bootstrap request {request_id} pending the owner's approval. Re-run \
                 `/memory import <ticket>` after they run `/sharing approve {request_id}`."
            )),
            Err(msg) => ExtensionCommandOutcome::Error(msg),
        }
    }

    /// Internal: perform the import flow for a ticket. Shared by
    /// `/memory import` and the ticket-aware `/memory attach` path so
    /// both routes converge on the same bootstrap + sync + index
    /// registration sequence.
    async fn import_bank_via_ticket(
        &self,
        ticket: &eidetica::sync::DatabaseTicket,
        permission: crate::commands::CoOwnerPermission,
    ) -> Result<ImportOutcome, String> {
        let db_id = ticket.database_id().clone();
        if let Some(entry) = self.memory_bank_index.find_by_id(&db_id) {
            return Ok(ImportOutcome::AlreadyLocal {
                display_name: entry.display_name,
            });
        }

        let eidetica_perm = match permission {
            crate::commands::CoOwnerPermission::Admin => {
                eidetica::auth::types::Permission::Admin(1)
            }
            crate::commands::CoOwnerPermission::Write => {
                eidetica::auth::types::Permission::Write(10)
            }
            crate::commands::CoOwnerPermission::Read => eidetica::auth::types::Permission::Read,
        };

        match self.registry.request_db_access(ticket, eidetica_perm).await {
            Ok(crate::session::BootstrapOutcome::Approved) => {}
            Ok(crate::session::BootstrapOutcome::Pending {
                request_id,
                message: _,
            }) => return Ok(ImportOutcome::Pending { request_id }),
            Err(e) => return Err(format!("Bootstrap failed: {e}")),
        }

        let bank_db = match self.registry.open_memory_bank(&db_id, None).await {
            Ok(Some(db)) => db,
            Ok(None) => {
                return Err(format!(
                    "Bootstrap reported success on memory bank {db_id} but this peer still holds \
                     no key. Likely an eidetica state mismatch — re-run the import to retry."
                ));
            }
            Err(e) => return Err(format!("Failed to open synced bank: {e}")),
        };

        let meta = bank_db
            .read_meta()
            .await
            .map_err(|e| format!("Failed to read bank meta: {e}"))?;
        let display_name = meta.display_name.clone().unwrap_or_else(|| {
            format!(
                "bank-{}",
                &db_id.to_string()[..8.min(db_id.to_string().len())]
            )
        });

        let pubkey = self
            .registry
            .find_key_for_db(&db_id)
            .await
            .map_err(|e| format!("Failed to look up bank key: {e}"))?
            .ok_or_else(|| {
                "Expected a key for this DB (open succeeded) but find_key returned None".to_string()
            })?;

        self.memory_bank_index
            .register(crate::hosted_index::DbEntry {
                db_id: db_id.clone(),
                display_name: display_name.clone(),
                pubkey,
            });

        if let Err(e) = self.registry.enable_sync_for(&db_id).await {
            return Err(format!(
                "Imported memory bank '{display_name}' (DB {db_id}) but failed to enable ongoing \
                 sync: {e}"
            ));
        }

        Ok(ImportOutcome::Imported {
            display_name,
            db_id,
        })
    }
}

/// Result of [`MemoryCommand::import_bank_via_ticket`]. Lets the
/// caller (`/memory import` or the ticket-aware `/memory attach`)
/// render appropriate messaging — successful import vs. pending
/// approval vs. already-local — and, for attach, get the resolved
/// display name to write into session settings.
enum ImportOutcome {
    Imported {
        display_name: String,
        db_id: eidetica::entry::ID,
    },
    AlreadyLocal {
        display_name: String,
    },
    Pending {
        request_id: String,
    },
}

impl MemoryCommand {
    /// `/memory attach <bank|db_id|ticket>` — attach a bank to the
    /// current session so its memories surface via auto-recall.
    ///
    /// Three argument shapes:
    /// * **Name** — must exist in the local bank index (created locally
    ///   or previously imported).
    /// * **DB ID** — resolved against the bank index; supports referring
    ///   to a bank without remembering its display name.
    /// * **DatabaseTicket** — bootstraps the bank from the issuing peer
    ///   first (default Write permission, matching `/memory import`),
    ///   then attaches the imported display name.
    ///
    /// We always store the resolved *display name* in
    /// `extension_settings["memory"]["attached_banks"]` so auto-recall's
    /// name-based lookup against `memory_bank_index` works regardless of
    /// what shape the user typed.
    async fn attach_cmd(&self, arg: &str, ctx: &HookContext) -> ExtensionCommandOutcome {
        if arg.is_empty() {
            return ExtensionCommandOutcome::Error(
                "Usage: /memory attach <bank|db_id|ticket>".into(),
            );
        }

        let (bank_name, prelude): (String, Option<String>) =
            if let Ok(ticket) = arg.parse::<eidetica::sync::DatabaseTicket>() {
                match self
                    .import_bank_via_ticket(&ticket, crate::commands::CoOwnerPermission::Write)
                    .await
                {
                    Ok(ImportOutcome::Imported {
                        display_name,
                        db_id,
                    }) => (
                        display_name.clone(),
                        Some(format!(
                            "Imported memory bank '{display_name}' (DB {db_id}) via ticket. \
                             Now attaching to this session."
                        )),
                    ),
                    Ok(ImportOutcome::AlreadyLocal { display_name }) => (display_name, None),
                    Ok(ImportOutcome::Pending { request_id }) => {
                        return ExtensionCommandOutcome::Text(format!(
                            "Bootstrap request {request_id} pending the owner's approval. \
                             Re-run `/memory attach <ticket>` after they run \
                             `/sharing approve {request_id}`."
                        ));
                    }
                    Err(msg) => return ExtensionCommandOutcome::Error(msg),
                }
            } else {
                match self.resolve_bank(arg) {
                    Ok(entry) => (entry.display_name, None),
                    Err(msg) => {
                        return ExtensionCommandOutcome::Error(format!(
                            "{msg}, or pass an eidetica DatabaseTicket URL to import + attach \
                             in one step"
                        ));
                    }
                }
            };

        let mut settings = ctx.get_settings("memory").await;
        let bank_json = serde_json::Value::String(bank_name.clone());
        let banks_arr = settings
            .as_object_mut()
            .and_then(|o| o.get_mut("attached_banks"))
            .and_then(|v| v.as_array_mut());

        match banks_arr {
            Some(arr) => {
                if arr.iter().any(|v| v == &bank_json) {
                    return ExtensionCommandOutcome::Text(format!(
                        "{}Bank '{bank_name}' is already attached to this session.",
                        prelude.map(|p| format!("{p}\n")).unwrap_or_default()
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
                "{}Attached bank '{bank_name}' to this session. Its memories will be surfaced \
                 in context.",
                prelude.map(|p| format!("{p}\n")).unwrap_or_default()
            )),
            Err(e) => ExtensionCommandOutcome::Error(format!("Failed to persist: {e}")),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::agent_db::{AgentDbConfig, AgentMeta};
    use crate::hosted_index::DbEntry;
    use crate::session::SessionRegistry;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;

    /// Build a MemoryCommand wired to an in-memory eidetica instance plus
    /// empty hosted indices. Returns the command and the registry so
    /// tests can seed agents/banks through the command itself.
    async fn fixture() -> (Instance, Arc<SessionRegistry>, MemoryCommand) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
        let agents = Arc::new(AgentRegistry::with_default_agent());
        let registry = Arc::new(
            SessionRegistry::new(instance.clone(), user, agents)
                .await
                .unwrap(),
        );
        let agent_index = HostedIndex::empty("agent");
        let memory_bank_index = HostedIndex::empty("bank");
        let cmd = MemoryCommand {
            registry: registry.clone(),
            agent_index,
            memory_bank_index,
        };
        (instance, registry, cmd)
    }

    /// Provision an agent through the registry and register it with the
    /// command's agent_index. Mirrors what `/agent new` would do.
    async fn seed_agent(registry: &SessionRegistry, cmd: &MemoryCommand, name: &str) {
        let (agent_db, pubkey) = registry
            .create_new_agent_db(
                name,
                &AgentDbConfig::default(),
                &AgentMeta {
                    display_name: Some(name.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        cmd.agent_index.register(DbEntry {
            db_id: agent_db.id(),
            display_name: name.into(),
            pubkey,
        });
    }

    fn assert_text(outcome: ExtensionCommandOutcome, needle: &str) {
        match outcome {
            ExtensionCommandOutcome::Text(s) => {
                assert!(s.contains(needle), "expected `{needle}` in `{s}`");
            }
            ExtensionCommandOutcome::Error(e) => panic!("unexpected error: {e}"),
        }
    }

    fn assert_error(outcome: ExtensionCommandOutcome, needle: &str) {
        match outcome {
            ExtensionCommandOutcome::Error(e) => {
                assert!(e.contains(needle), "expected `{needle}` in error `{e}`");
            }
            ExtensionCommandOutcome::Text(s) => panic!("expected error, got: {s}"),
        }
    }

    #[tokio::test]
    async fn new_cmd_creates_and_registers_bank() {
        let (_i, _r, cmd) = fixture().await;
        assert_text(cmd.new_cmd("patrick notes about Patrick").await, "patrick");
        let banks = cmd.memory_bank_index.list();
        assert_eq!(banks.len(), 1);
        assert_eq!(banks[0].display_name, "patrick");
    }

    #[tokio::test]
    async fn new_cmd_rejects_duplicate_name() {
        let (_i, _r, cmd) = fixture().await;
        cmd.new_cmd("patrick").await;
        assert_error(cmd.new_cmd("patrick").await, "already exists");
    }

    #[tokio::test]
    async fn list_cmd_shows_created_banks() {
        let (_i, _r, cmd) = fixture().await;
        assert_text(cmd.list_cmd().await, "No memory banks");
        for name in ["patrick", "projects"] {
            cmd.new_cmd(name).await;
        }
        match cmd.list_cmd().await {
            ExtensionCommandOutcome::Text(s) => {
                assert!(s.contains("patrick"), "missing patrick: {s}");
                assert!(s.contains("projects"), "missing projects: {s}");
            }
            ExtensionCommandOutcome::Error(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn delete_cmd_unregisters_but_preserves_db() {
        let (_i, registry, cmd) = fixture().await;
        cmd.new_cmd("patrick").await;
        let db_id = cmd.memory_bank_index.find_by_name("patrick").unwrap().db_id;

        assert_text(cmd.delete_cmd("patrick").await, "Deleted");
        assert!(cmd.memory_bank_index.find_by_name("patrick").is_none());

        // DB itself is still openable (archive preserved).
        assert!(
            registry
                .open_memory_bank(&db_id, None)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn delete_cmd_unknown_errors() {
        let (_i, _r, cmd) = fixture().await;
        assert_error(cmd.delete_cmd("ghost").await, "No hosted memory bank");
    }

    #[tokio::test]
    async fn grant_cmd_writes_auth_and_ref() {
        let (_i, registry, cmd) = fixture().await;
        seed_agent(&registry, &cmd, "alpha").await;
        cmd.new_cmd("patrick").await;
        let agent_db_id = cmd.agent_index.find_by_name("alpha").unwrap().db_id;
        let bank_db_id = cmd.memory_bank_index.find_by_name("patrick").unwrap().db_id;

        match cmd.grant_cmd("patrick alpha write").await {
            ExtensionCommandOutcome::Text(s) => {
                assert!(s.contains("patrick"));
                assert!(s.contains("Write"));
            }
            ExtensionCommandOutcome::Error(e) => panic!("unexpected: {e}"),
        }

        let agent_db = registry
            .open_agent_db(&agent_db_id, None)
            .await
            .unwrap()
            .unwrap();
        let banks = agent_db.list_memory_banks().await.unwrap();
        assert_eq!(banks.len(), 1);
        assert_eq!(banks[0].name, "patrick");
        assert_eq!(banks[0].db_id, bank_db_id.to_string());
        assert_eq!(banks[0].permission, crate::agent_db::BankPermission::Write);
    }

    #[tokio::test]
    async fn revoke_cmd_reverses_grant() {
        let (_i, registry, cmd) = fixture().await;
        seed_agent(&registry, &cmd, "alpha").await;
        cmd.new_cmd("patrick").await;
        let agent_db_id = cmd.agent_index.find_by_name("alpha").unwrap().db_id;
        cmd.grant_cmd("patrick alpha read").await;

        let agent_db = registry
            .open_agent_db(&agent_db_id, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(agent_db.list_memory_banks().await.unwrap().len(), 1);

        assert_text(cmd.revoke_cmd("patrick alpha").await, "Revoked");

        let agent_db = registry
            .open_agent_db(&agent_db_id, None)
            .await
            .unwrap()
            .unwrap();
        assert!(agent_db.list_memory_banks().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn share_cmd_unknown_bank_errors() {
        let (_i, _r, cmd) = fixture().await;
        assert_error(cmd.share_cmd("ghost").await, "No hosted memory bank");
    }

    #[tokio::test]
    async fn import_cmd_rejects_invalid_ticket() {
        let (_i, _r, cmd) = fixture().await;
        // Sync may or may not be enabled in the fixture; either path surfaces
        // a clean Error.
        match cmd.import_cmd("not-a-ticket").await {
            ExtensionCommandOutcome::Error(e) => {
                assert!(
                    e.contains("Invalid ticket") || e.contains("Sync not enabled"),
                    "got {e}"
                );
            }
            ExtensionCommandOutcome::Text(s) => panic!("expected error, got: {s}"),
        }
    }

    #[tokio::test]
    async fn grant_cmd_unknown_bank_errors() {
        let (_i, registry, cmd) = fixture().await;
        seed_agent(&registry, &cmd, "alpha").await;
        assert_error(
            cmd.grant_cmd("nope alpha read").await,
            "No hosted memory bank",
        );
    }

    /// Bank-name resolution is duplicated against the agent path — make
    /// sure the wrong index isn't accepted by mistake.
    #[tokio::test]
    async fn grant_cmd_bank_and_agent_indices_are_disjoint() {
        let (_i, registry, cmd) = fixture().await;
        seed_agent(&registry, &cmd, "alpha").await;
        // No bank named "alpha" exists, even though an agent does.
        assert_error(
            cmd.grant_cmd("alpha alpha write").await,
            "No hosted memory bank",
        );
    }

    #[tokio::test]
    async fn new_cmd_rejects_empty_name() {
        let (_i, _r, cmd) = fixture().await;
        assert_error(cmd.new_cmd("").await, "name required");
    }

    #[tokio::test]
    async fn grant_cmd_rejects_bad_permission() {
        let (_i, registry, cmd) = fixture().await;
        seed_agent(&registry, &cmd, "alpha").await;
        cmd.new_cmd("patrick").await;
        assert_error(
            cmd.grant_cmd("patrick alpha wat").await,
            "Unknown permission",
        );
    }

    // ── MemoryAccess cap: scope plumbing ─────────────────────────────

    /// Build a MemoryAccess provider over the same registry/indices that
    /// the command fixture uses, so a single test can populate via the
    /// command surface and then assert via the cap.
    fn access_for(cmd: &MemoryCommand) -> MemoryAccessImpl {
        MemoryAccessImpl {
            registry: cmd.registry.clone(),
            agent_index: cmd.agent_index.clone(),
            memory_bank_index: cmd.memory_bank_index.clone(),
            embedder: None,
        }
    }

    #[tokio::test]
    async fn memory_access_agent_scope_reads_and_writes_own_memory() {
        let (_i, registry, cmd) = fixture().await;
        seed_agent(&registry, &cmd, "alpha").await;
        let access = access_for(&cmd);

        access
            .remember("alpha", "color", "teal", MemoryScope::Agent)
            .await
            .unwrap();
        let hits = access
            .search("alpha", "color", MemoryScope::Agent)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "color");
        assert_eq!(hits[0].value, "teal");
        assert!(hits[0].bank.is_none(), "agent scope hits carry no bank");
        assert!(hits[0].score > 0.0, "score should propagate");
    }

    #[tokio::test]
    async fn memory_access_bank_scope_tags_hits_with_bank_name() {
        let (_i, registry, cmd) = fixture().await;
        seed_agent(&registry, &cmd, "alpha").await;
        cmd.new_cmd("shared").await;
        cmd.grant_cmd("shared alpha write").await;
        let access = access_for(&cmd);

        access
            .remember(
                "alpha",
                "passphrase",
                "purple-narwhal-quartz",
                MemoryScope::Bank {
                    name: "shared".into(),
                },
            )
            .await
            .unwrap();
        let hits = access
            .search(
                "alpha",
                "passphrase",
                MemoryScope::Bank {
                    name: "shared".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value, "purple-narwhal-quartz");
        assert_eq!(
            hits[0].bank.as_deref(),
            Some("shared"),
            "bank scope hits carry the bank label"
        );
    }

    #[tokio::test]
    async fn memory_access_agent_scope_unknown_agent_errors() {
        let (_i, _r, cmd) = fixture().await;
        let access = access_for(&cmd);
        let err = access
            .search("ghost", "anything", MemoryScope::Agent)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("agent not found"), "got {err}");
    }

    // ── /memory attach: ticket / name / DB ID resolution ─────────────

    async fn attach_via_dispatcher(cmd: &MemoryCommand, arg: &str) -> ExtensionCommandOutcome {
        // Build a HookContext bound to a real session DB so the
        // attach handler's get_settings/set_settings have somewhere to
        // land.
        use crate::session::Session;
        use crate::types::ConversationId;
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;
        let (_conv, session_db) = cmd.registry.create_session(Some("test")).await.unwrap();
        let session = Arc::new(TokioMutex::new(
            Session::new(ConversationId(session_db.root_id().to_string()), session_db).await,
        ));
        let ctx = HookContext {
            agent_name: "alpha".to_string(),
            model: None,
            call_depth: 0,
            session,
            active_extensions: std::collections::HashSet::new(),
        };
        cmd.attach_cmd(arg, &ctx).await
    }

    #[tokio::test]
    async fn attach_cmd_resolves_name() {
        let (_i, _r, cmd) = fixture().await;
        cmd.new_cmd("patrick").await;
        assert_text(
            attach_via_dispatcher(&cmd, "patrick").await,
            "Attached bank 'patrick'",
        );
    }

    #[tokio::test]
    async fn attach_cmd_resolves_db_id() {
        let (_i, _r, cmd) = fixture().await;
        cmd.new_cmd("patrick").await;
        let db_id = cmd
            .memory_bank_index
            .find_by_name("patrick")
            .unwrap()
            .db_id
            .to_string();
        // Attach by raw DB ID — should resolve to the display name and
        // attach as 'patrick'.
        assert_text(
            attach_via_dispatcher(&cmd, &db_id).await,
            "Attached bank 'patrick'",
        );
    }

    #[tokio::test]
    async fn attach_cmd_unknown_arg_suggests_ticket() {
        let (_i, _r, cmd) = fixture().await;
        match attach_via_dispatcher(&cmd, "ghost-bank").await {
            ExtensionCommandOutcome::Error(msg) => {
                assert!(
                    msg.contains("No hosted memory bank") && msg.contains("DatabaseTicket"),
                    "error should mention ticket fallback: {msg}"
                );
            }
            ExtensionCommandOutcome::Text(s) => panic!("expected error, got: {s}"),
        }
    }

    #[tokio::test]
    async fn attach_cmd_idempotent_on_repeat() {
        let (_i, _r, cmd) = fixture().await;
        cmd.new_cmd("patrick").await;
        // Use the same session for both calls so we exercise the
        // "already attached" branch.
        use crate::session::Session;
        use crate::types::ConversationId;
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;
        let (_conv, session_db) = cmd.registry.create_session(Some("t")).await.unwrap();
        let session = Arc::new(TokioMutex::new(
            Session::new(ConversationId(session_db.root_id().to_string()), session_db).await,
        ));
        let ctx = HookContext {
            agent_name: "alpha".to_string(),
            model: None,
            call_depth: 0,
            session,
            active_extensions: std::collections::HashSet::new(),
        };
        cmd.attach_cmd("patrick", &ctx).await;
        match cmd.attach_cmd("patrick", &ctx).await {
            ExtensionCommandOutcome::Text(s) => {
                assert!(s.contains("already attached"), "got {s}");
            }
            ExtensionCommandOutcome::Error(e) => panic!("unexpected: {e}"),
        }
    }
}
