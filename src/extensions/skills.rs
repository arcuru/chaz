//! Skills extension — manages a catalog of SKILL.md files and injects
//! matched skill bodies into the agent's system prompt via the
//! [`crate::extension::caps::PromptAugmentation`] capability.
//!
//! Skill directories (scanned at install time, highest priority first):
//! 1. `.chaz/skills/` — project-local (relative to cwd)
//! 2. `~/.config/chaz/skills/` — user-global
//!
//! SKILL.md format: YAML frontmatter + Markdown body.

use crate::extension::caps::{
    CapabilityKind, CapabilityRequest, CommandDescriptor, ExtensionCaps, PromptAugmentation,
};
use crate::extension::handler::InstalledExtension;
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{
    Extension, ExtensionCommand, ExtensionCommandOutcome, ExtensionRef, HookContext, HookKind,
};
use crate::hosted_index::HostedIndex;
use crate::session::SessionRegistry;
use crate::tool::{RiskLevel, Tool, ToolDescriptor, ToolError, ToolPolicy};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

/// Parsed SKILL.md file.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub body: String,
    #[allow(dead_code)]
    pub source_dir: PathBuf,
}

/// In-memory skill catalog built at startup from disk.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self { skills: Vec::new() }
    }

    /// Scan skill directories, parse SKILL.md files, populate the registry.
    pub fn scan(&mut self) {
        let mut seen: HashMap<String, usize> = HashMap::new();
        let dirs: Vec<PathBuf> = vec![PathBuf::from(".chaz/skills"), dirs_fallback()];

        for dir in &dirs {
            if !dir.is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                match parse_skill_md(&path) {
                    Ok(skill) => {
                        if seen.contains_key(&skill.name) {
                            continue;
                        }
                        seen.insert(skill.name.clone(), self.skills.len());
                        self.skills.push(skill);
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            "Failed to parse SKILL.md: {e}"
                        );
                    }
                }
            }
        }

        tracing::info!(count = self.skills.len(), "Skills loaded");
    }

    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    pub fn search(&self, query: &str) -> Vec<&Skill> {
        let q = query.to_lowercase();
        self.skills
            .iter()
            .filter(|s| {
                s.name.to_lowercase().contains(&q)
                    || s.description.to_lowercase().contains(&q)
                    || s.triggers.iter().any(|t| t.contains(&q))
            })
            .collect()
    }
}

fn dirs_fallback() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/chaz/skills")
    } else {
        PathBuf::from("/tmp/chaz-skills")
    }
}

fn parse_skill_md(path: &PathBuf) -> anyhow::Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    if content.len() > 65536 {
        anyhow::bail!("SKILL.md exceeds 64 KiB limit");
    }

    let trimmed = content.trim_start();
    let Some(remaining) = trimmed.strip_prefix("---\n") else {
        anyhow::bail!("missing YAML frontmatter (must start with ---)");
    };

    let Some((frontmatter, body)) = remaining.split_once("\n---") else {
        anyhow::bail!("unclosed YAML frontmatter (missing closing ---)");
    };

    let fm: SkillFrontmatter = serde_yaml::from_str(frontmatter)?;

    Ok(Skill {
        name: fm.name,
        description: fm.description.unwrap_or_default(),
        triggers: fm.triggers.unwrap_or_default(),
        body: body.trim().to_string(),
        source_dir: path.parent().unwrap_or(&PathBuf::from(".")).to_path_buf(),
    })
}

#[derive(serde::Deserialize)]
struct SkillFrontmatter {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    triggers: Option<Vec<String>>,
}

// ── Extension implementation ────────────────────────────────────────

pub struct SkillsExtension {
    /// In-memory catalog scanned from disk at install time. Coexists
    /// with eidetica-backed skills (per-agent + bank-attached) until
    /// the PerSession migration unifies all four sources into one
    /// composed view.
    disk_registry: Arc<std::sync::RwLock<SkillRegistry>>,
    /// Session registry for opening agent / bank DBs from the slash
    /// surface.
    session_registry: Arc<SessionRegistry>,
    /// Hosted index of locally tracked agents (for grant/revoke
    /// resolution).
    agent_index: HostedIndex,
    /// Hosted index of locally tracked skill banks (for /skills
    /// list / grant / revoke / share / unshare / import / attach).
    skill_bank_index: HostedIndex,
}

impl SkillsExtension {
    pub fn new(
        session_registry: Arc<SessionRegistry>,
        agent_index: HostedIndex,
        skill_bank_index: HostedIndex,
    ) -> Self {
        Self {
            disk_registry: Arc::new(std::sync::RwLock::new(SkillRegistry::new())),
            session_registry,
            agent_index,
            skill_bank_index,
        }
    }
}

impl Extension for SkillsExtension {
    fn name(&self) -> &'static str {
        "skills"
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
            provides_capabilities: vec![CapabilityKind::PromptAugmentation],
        }
    }

    fn install<'a>(
        &'a self,
        caps: ExtensionCaps,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<InstalledExtension>> + Send + 'a>> {
        let skill_count = {
            let mut registry = self.disk_registry.write().unwrap();
            registry.scan();
            registry.list().len()
        };

        Box::pin(async move {
            let tool_reg = caps
                .tool_registration
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("skills install requires ToolRegistration cap"))?;
            let cmd_reg = caps.command_registration.as_ref().ok_or_else(|| {
                anyhow::anyhow!("skills install requires CommandRegistration cap")
            })?;

            let tools: Vec<Arc<dyn Tool>> = vec![
                Arc::new(SkillListTool {
                    registry: self.disk_registry.clone(),
                }),
                Arc::new(SkillSearchTool {
                    registry: self.disk_registry.clone(),
                }),
                Arc::new(SkillShowTool {
                    disk: self.disk_registry.clone(),
                    registry: self.session_registry.clone(),
                    agent_index: self.agent_index.clone(),
                    skill_bank_index: self.skill_bank_index.clone(),
                }),
            ];
            for t in &tools {
                let d = t.descriptor();
                tool_reg.register(d, t.clone()).await?;
            }

            cmd_reg
                .register(
                    CommandDescriptor {
                        name: "skills".into(),
                        description: "Manage skill banks: list | new | delete | grant | revoke | \
                                      share | unshare | import | attach <name|db_id|ticket> | \
                                      detach"
                            .into(),
                    },
                    Box::new(SkillCommand {
                        registry: self.session_registry.clone(),
                        agent_index: self.agent_index.clone(),
                        skill_bank_index: self.skill_bank_index.clone(),
                    }),
                )
                .await?;

            tracing::info!(count = skill_count, "Skills extension installed");
            Ok(InstalledExtension::empty())
        })
    }

    // The skills extension is Scope::PerSession (mirror memory). The
    // catalog snapshot captured at instantiate time is what the
    // PromptAugmentation endpoint reads. No global ContextTail /
    // PromptAugmentation provider — selection is per-session because
    // session_attached_banks are.

    fn scope(&self) -> crate::extension::Scope {
        crate::extension::Scope::PerSession
    }

    fn instantiate<'a>(
        &'a self,
        scope_ctx: crate::extension::ScopeCtx<'a>,
    ) -> crate::extension::instance::InstantiateFuture<'a> {
        let disk = self.disk_registry.clone();
        let registry = self.session_registry.clone();
        let agent_index = self.agent_index.clone();
        let skill_bank_index = self.skill_bank_index.clone();
        let manifest = self.manifest();
        Box::pin(async move {
            // Session-attached banks live in extension_settings["skills"].
            // For non-Session scope contexts (the install-time Global
            // dry-run via `LegacyInstance`), there's no session DB —
            // we end up with an empty attached list, which means the
            // catalog at call time will still compose disk + agent
            // sources.
            let session_attached_banks: Vec<String> = match &scope_ctx {
                crate::extension::ScopeCtx::Session { session_db, .. } => {
                    let settings = crate::extension::read_settings(session_db, "skills").await;
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

            let inputs = Arc::new(SessionSkills {
                disk,
                registry,
                agent_index,
                skill_bank_index,
                session_attached_banks,
            });

            Ok(Arc::new(SkillsInstance { manifest, inputs })
                as Arc<dyn crate::extension::ExtensionInstance>)
        })
    }
}

// ── Per-session instance ─────────────────────────────────────────────

struct SkillsInstance {
    manifest: ExtensionManifest,
    inputs: Arc<SessionSkills>,
}

impl crate::extension::ExtensionInstance for SkillsInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn prompt_augmentation(&self) -> Option<Arc<dyn PromptAugmentation>> {
        Some(Arc::new(SkillsPromptAugmentation {
            inputs: self.inputs.clone(),
        }))
    }
}

// ── PromptAugmentation impl ─────────────────────────────────────────
//
// Per the agentskills.io spec ("progressive disclosure"), we inject
// the *catalog* — name + description per available skill — into the
// system prompt. The LLM decides from descriptions when to invoke a
// skill; activation happens via the `skill_show` tool, which fetches
// the full body on demand. No host-side keyword matching or relevance
// scoring — selection is the LLM's job.

/// One catalog entry surfaced in the system prompt and resolvable by
/// `skill_show`. The four sources (disk + agent.skills + granted
/// banks + session-attached banks) are composed into a single
/// `CatalogEntry` list at PromptAugmentation call time.
#[derive(Debug, Clone)]
pub(crate) struct CatalogEntry {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Provenance label — kept for future UI / debugging surfaces.
    /// Not part of the agentskills.io spec and deliberately not
    /// surfaced in the catalog block injected into the system prompt.
    #[allow(dead_code)]
    pub source: SkillSource,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // see CatalogEntry::source.
pub(crate) enum SkillSource {
    /// Loaded from a `.chaz/skills/` or `~/.config/chaz/skills/` dir.
    Disk,
    /// Loaded from `AgentDb.skills` for the active agent.
    Agent,
    /// Loaded from a `SkillBankDb` the agent has been granted access to.
    Bank { name: String },
    /// Loaded from a `SkillBankDb` attached for this session only.
    SessionBank { name: String },
}

/// Compose a catalog from the four layers. Names dedupe across layers
/// — first one wins. Walking order matters: disk → agent → granted
/// banks → session-attached banks, with later sources NOT overriding
/// earlier ones (rationale: disk is the most stable, session-attached
/// is the most transient; a name collision is almost certainly an
/// accident, and surfacing the stable definition is the safer default).
async fn compose_catalog(
    disk: &Arc<std::sync::RwLock<SkillRegistry>>,
    registry: &SessionRegistry,
    agent_index: &HostedIndex,
    skill_bank_index: &HostedIndex,
    agent_name: &str,
    session_attached_banks: &[String],
) -> Vec<CatalogEntry> {
    use std::collections::HashSet;
    let mut entries: Vec<CatalogEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1. Disk
    {
        let r = disk.read().unwrap();
        for s in r.list() {
            if seen.insert(s.name.clone()) {
                entries.push(CatalogEntry {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    body: s.body.clone(),
                    source: SkillSource::Disk,
                });
            }
        }
    }

    // 2. AgentDb.skills (this agent's own private skills)
    if let Some(agent_entry) = agent_index.find_by_name(agent_name)
        && let Ok(Some(agent_db)) = registry
            .open_agent_db(&agent_entry.db_id, Some(&agent_entry.pubkey))
            .await
    {
        for s in read_skill_rows(agent_db.database()).await {
            if seen.insert(s.name.clone()) {
                entries.push(CatalogEntry {
                    name: s.name,
                    description: s.description,
                    body: s.body,
                    source: SkillSource::Agent,
                });
            }
        }

        // 3. Granted SkillBankDbs (per-agent persistent grants)
        if let Ok(bank_refs) = agent_db.list_skill_banks().await {
            for bref in bank_refs {
                let Some(bank_entry) = skill_bank_index.find_by_name(&bref.name) else {
                    continue;
                };
                let Ok(Some(bank)) = registry
                    .open_skill_bank(&bank_entry.db_id, Some(&bank_entry.pubkey))
                    .await
                else {
                    continue;
                };
                for s in read_skill_rows(bank.database()).await {
                    if seen.insert(s.name.clone()) {
                        entries.push(CatalogEntry {
                            name: s.name,
                            description: s.description,
                            body: s.body,
                            source: SkillSource::Bank {
                                name: bref.name.clone(),
                            },
                        });
                    }
                }
            }
        }
    }

    // 4. Session-attached banks (transient)
    for bank_name in session_attached_banks {
        let Some(bank_entry) = skill_bank_index.find_by_name(bank_name) else {
            continue;
        };
        let Ok(Some(bank)) = registry
            .open_skill_bank(&bank_entry.db_id, Some(&bank_entry.pubkey))
            .await
        else {
            continue;
        };
        for s in read_skill_rows(bank.database()).await {
            if seen.insert(s.name.clone()) {
                entries.push(CatalogEntry {
                    name: s.name,
                    description: s.description,
                    body: s.body,
                    source: SkillSource::SessionBank {
                        name: bank_name.clone(),
                    },
                });
            }
        }
    }

    entries
}

/// Pull every `Skill` row from a database's `skills` store. Returns
/// an empty Vec on any read error (storage layer is best-effort here —
/// missing skill rows shouldn't fail the whole augmentation).
async fn read_skill_rows(db: &eidetica::Database) -> Vec<crate::agent_db::Skill> {
    use eidetica::store::Table;
    let Ok(txn) = db.new_transaction().await else {
        return Vec::new();
    };
    let Ok(store) = txn
        .get_store::<Table<crate::agent_db::Skill>>(crate::agent_db::SKILLS_STORE)
        .await
    else {
        return Vec::new();
    };
    match store.search(|_: &crate::agent_db::Skill| true).await {
        Ok(rows) => rows.into_iter().map(|(_, s)| s).collect(),
        Err(_) => Vec::new(),
    }
}

/// Format the catalog as the Markdown block injected into the system
/// prompt. Discovery-only — names + descriptions, no bodies.
fn format_catalog(entries: &[CatalogEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut out = String::from("## Available skills\n");
    out.push_str(
        "Each line is `name — description`. To use a skill, call the `skill_show` tool with the \
         skill's `name` to load its full instructions.\n\n",
    );
    for e in entries {
        out.push_str(&format!("- **{}** — {}\n", e.name, e.description));
    }
    Some(out)
}

/// Per-session inputs to catalog composition. Built at session_start
/// by `SkillsExtension::instantiate(Session)`. The disk registry +
/// hosted-index handles are immutable across the session; only
/// `session_attached_banks` reflects per-session state (captured at
/// instantiate, refreshed at next session_start if the user runs
/// `/skills attach` mid-session).
///
/// The catalog itself is composed at PromptAugmentation call time —
/// that's when we know the active agent (via the `agent_name`
/// parameter), and AgentDb.skills depends on it. This mirrors how
/// `MemoryContextTail` defers per-agent reads to call time.
pub(crate) struct SessionSkills {
    pub disk: Arc<std::sync::RwLock<SkillRegistry>>,
    pub registry: Arc<SessionRegistry>,
    pub agent_index: HostedIndex,
    pub skill_bank_index: HostedIndex,
    pub session_attached_banks: Vec<String>,
}

impl SessionSkills {
    pub async fn catalog_for(&self, agent_name: &str) -> Vec<CatalogEntry> {
        compose_catalog(
            &self.disk,
            &self.registry,
            &self.agent_index,
            &self.skill_bank_index,
            agent_name,
            &self.session_attached_banks,
        )
        .await
    }
}

/// PromptAugmentation backed by per-session inputs. Composes the
/// catalog at call time using the active agent's identity.
struct SkillsPromptAugmentation {
    inputs: Arc<SessionSkills>,
}

impl PromptAugmentation for SkillsPromptAugmentation {
    fn augment_system_prompt<'a>(
        &'a self,
        agent_name: &'a str,
        _recent_message_text: &'a [String],
    ) -> crate::extension::caps::CapFuture<'a, Option<String>> {
        let inputs = self.inputs.clone();
        let agent = agent_name.to_string();
        Box::pin(async move {
            let entries = inputs.catalog_for(&agent).await;
            Ok(format_catalog(&entries))
        })
    }
}

// ── Skill management tools ──────────────────────────────────────────

struct SkillListTool {
    registry: Arc<std::sync::RwLock<SkillRegistry>>,
}

impl Tool for SkillListTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "skill_list".into(),
            description: "List available skills with name and description".into(),
            parameters: serde_json::json!({}),
        }
    }

    fn execute<'a>(
        &'a self,
        _arguments: Value,
        _ctx: &'a crate::tool::ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        let items: Vec<String> = {
            let registry = self.registry.read().unwrap();
            let skills = registry.list();
            skills
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    format!(
                        "{}. **{}** — {} (triggers: {})\n",
                        i + 1,
                        s.name,
                        s.description,
                        s.triggers.join(", ")
                    )
                })
                .collect()
        };
        Box::pin(async move {
            if items.is_empty() {
                Ok("(no skills loaded)".to_string())
            } else {
                Ok(items.join(""))
            }
        })
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Low,
            ..ToolPolicy::default()
        }
    }
}

struct SkillSearchTool {
    registry: Arc<std::sync::RwLock<SkillRegistry>>,
}

impl Tool for SkillSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "skill_search".into(),
            description: "Search available skills by name, description, or trigger keyword".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search term" }
                },
                "required": ["query"]
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        _ctx: &'a crate::tool::ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        let query = arguments["query"].as_str().unwrap_or("").to_string();
        let items: Vec<String> = {
            let registry = self.registry.read().unwrap();
            let results = registry.search(&query);
            results
                .iter()
                .enumerate()
                .map(|(i, s)| format!("{}. **{}** — {}\n", i + 1, s.name, s.description))
                .collect()
        };
        Box::pin(async move {
            if items.is_empty() {
                Ok(format!("No skills matching \"{query}\""))
            } else {
                Ok(items.join(""))
            }
        })
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Low,
            ..ToolPolicy::default()
        }
    }
}

/// `skill_show` — the "activation" half of progressive disclosure.
/// Resolves a skill name against all four sources (disk + agent's
/// own + granted banks + session-attached banks) and returns the
/// full body JSON for the LLM to execute against.
struct SkillShowTool {
    disk: Arc<std::sync::RwLock<SkillRegistry>>,
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    skill_bank_index: HostedIndex,
}

impl Tool for SkillShowTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "skill_show".into(),
            description: "Display the full content of a named skill".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Skill name" }
                },
                "required": ["name"]
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a crate::tool::ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        let name = arguments["name"].as_str().unwrap_or("").to_string();
        let agent = ctx.agent_name.clone();
        let session = ctx.session.clone();
        Box::pin(async move {
            // Session-attached banks aren't held on the tool instance
            // (the tool is registered globally; the per-session
            // SkillsInstance is the canonical home for them). Read
            // them here at call time from the active session DB.
            let session_attached_banks: Vec<String> = {
                let session = session.lock().await;
                let db = session.database();
                let settings = crate::extension::read_settings(db, "skills").await;
                settings
                    .get("attached_banks")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            };

            // Walk the same source order as compose_catalog so
            // discovery and activation agree on which definition wins.
            let entries = compose_catalog(
                &self.disk,
                &self.registry,
                &self.agent_index,
                &self.skill_bank_index,
                &agent,
                &session_attached_banks,
            )
            .await;
            match entries.iter().find(|e| e.name == name) {
                Some(entry) => Ok(serde_json::json!({
                    "text": entry.body,
                    "name": entry.name,
                    "description": entry.description,
                })
                .to_string()),
                None => Ok(format!("No skill named \"{name}\" found.")),
            }
        })
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::Low,
            ..ToolPolicy::default()
        }
    }
}

// ── /skills slash surface (mirror of /memory) ────────────────────────
//
// Bank-level CRUD + sharing + per-session attach/detach. The shape is
// a near-clone of `MemoryCommand` in src/extensions/memory.rs — same
// arg-shape, same outcomes, just routed at SkillBankDb instead of
// MemoryBankDb. Adding skills *to* a bank comes via a separate tool
// (planned), the same way `remember` writes to memory banks.

struct SkillCommand {
    registry: Arc<SessionRegistry>,
    agent_index: HostedIndex,
    skill_bank_index: HostedIndex,
}

impl ExtensionCommand for SkillCommand {
    fn description(&self) -> &'static str {
        "Manage skill banks: list | new | delete | grant | revoke | share | unshare | import | \
         attach <name|db_id|ticket> | detach"
    }

    fn invoke<'a>(
        &'a self,
        args: &'a str,
        ctx: &'a HookContext,
    ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
        Box::pin(async move {
            let args = args.trim();
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
            if let Some(arg) = args.strip_prefix("attach ") {
                return self.attach_cmd(arg.trim(), ctx).await;
            }
            if let Some(bank_name) = args.strip_prefix("detach ") {
                return detach_cmd(bank_name.trim(), ctx).await;
            }
            ExtensionCommandOutcome::Error(format!(
                "Unknown skills sub-command: '{args}'. Use: list | new <name> [desc] | \
                 delete <bank> | grant <bank> <agent> <read|write> | revoke <bank> <agent> | \
                 share <bank> | unshare <bank> | import <ticket> [admin|write|read] | \
                 attach <bank|db_id|ticket> | detach <bank>"
            ))
        })
    }
}

impl SkillCommand {
    fn resolve_bank(&self, bank_ref: &str) -> Result<crate::hosted_index::DbEntry, String> {
        if let Some(entry) = self.skill_bank_index.find_by_name(bank_ref) {
            return Ok(entry);
        }
        if let Ok(id) = eidetica::entry::ID::parse(bank_ref)
            && let Some(entry) = self.skill_bank_index.find_by_id(&id)
        {
            return Ok(entry);
        }
        Err(format!(
            "No hosted skill bank matches '{bank_ref}' (try a display name from /skills list \
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
        let entries = self.skill_bank_index.list();
        if entries.is_empty() {
            return ExtensionCommandOutcome::Text(
                "No skill banks on this peer. Create one with /skills new <name>.".into(),
            );
        }
        let lines: Vec<String> = entries
            .iter()
            .map(|e| format!("  {} ({})", e.display_name, e.db_id))
            .collect();
        ExtensionCommandOutcome::Text(format!("Skill banks on this peer:\n{}", lines.join("\n")))
    }

    async fn new_cmd(&self, rest: &str) -> ExtensionCommandOutcome {
        let (name, desc) = match rest.split_once(char::is_whitespace) {
            Some((n, d)) => (n.trim(), Some(d.trim().to_string())),
            None => (rest, None),
        };
        let desc = desc.filter(|s| !s.is_empty());
        if name.is_empty() {
            return ExtensionCommandOutcome::Error("Skill bank name required".into());
        }
        let meta = crate::skill_bank_db::SkillBankMeta {
            display_name: Some(name.to_string()),
            description: desc,
        };
        let (bank, pubkey) = match self.registry.create_new_skill_bank(name, &meta).await {
            Ok(p) => p,
            Err(e) => {
                return ExtensionCommandOutcome::Error(format!("Failed to create skill bank: {e}"));
            }
        };
        self.skill_bank_index
            .register(crate::hosted_index::DbEntry {
                db_id: bank.id(),
                display_name: name.to_string(),
                pubkey,
            });
        ExtensionCommandOutcome::Text(format!(
            "Created skill bank '{name}' (DB {}). Grant it to an agent with /skills grant.",
            bank.id()
        ))
    }

    async fn delete_cmd(&self, bank_ref: &str) -> ExtensionCommandOutcome {
        if bank_ref.is_empty() {
            return ExtensionCommandOutcome::Error("Usage: /skills delete <name|db_id>".into());
        }
        let entry = match self.resolve_bank(bank_ref) {
            Ok(e) => e,
            Err(msg) => return ExtensionCommandOutcome::Error(msg),
        };
        self.skill_bank_index.unregister(&entry.db_id);
        ExtensionCommandOutcome::Text(format!(
            "Deleted skill bank '{}' (DB {} preserved for archive). Agents with this bank in \
             their skill_banks subtree will still see it listed — use /skills revoke to remove \
             grants.",
            entry.display_name, entry.db_id
        ))
    }

    async fn grant_cmd(&self, rest: &str) -> ExtensionCommandOutcome {
        let mut parts = rest.splitn(3, char::is_whitespace);
        let bank_ref = parts.next().unwrap_or("").trim();
        let agent_ref = parts.next().unwrap_or("").trim();
        let perm_tok = parts.next().unwrap_or("").trim();
        if bank_ref.is_empty() || agent_ref.is_empty() || perm_tok.is_empty() {
            return ExtensionCommandOutcome::Error(
                "Usage: /skills grant <bank> <agent> <read|write>".into(),
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
        let key_label = format!("skill:{}:{}", bank.display_name, agent.display_name);
        if let Err(e) = self
            .registry
            .grant_on_skill_bank(&bank.db_id, &agent.pubkey, &key_label, permission)
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
                    .revoke_on_skill_bank(&bank.db_id, &agent.pubkey)
                    .await;
                return ExtensionCommandOutcome::Error(format!(
                    "Granted auth but can't open agent '{}'s DB to record the ref — rolled back",
                    agent.display_name
                ));
            }
            Err(e) => {
                let _ = self
                    .registry
                    .revoke_on_skill_bank(&bank.db_id, &agent.pubkey)
                    .await;
                return ExtensionCommandOutcome::Error(format!(
                    "Granted auth but failed to open agent DB — rolled back: {e}"
                ));
            }
        };
        let ref_entry = crate::agent_db::SkillBankRef {
            name: bank.display_name.clone(),
            db_id: bank.db_id.to_string(),
            permission,
        };
        if let Err(e) = agent_db.attach_skill_bank(ref_entry).await {
            let _ = self
                .registry
                .revoke_on_skill_bank(&bank.db_id, &agent.pubkey)
                .await;
            return ExtensionCommandOutcome::Error(format!(
                "Granted auth but failed to write ref to agent DB — rolled back: {e}"
            ));
        }
        ExtensionCommandOutcome::Text(format!(
            "Granted agent '{}' {:?} access to skill bank '{}'",
            agent.display_name, permission, bank.display_name
        ))
    }

    async fn revoke_cmd(&self, rest: &str) -> ExtensionCommandOutcome {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let bank_ref = parts.next().unwrap_or("").trim();
        let agent_ref = parts.next().unwrap_or("").trim();
        if bank_ref.is_empty() || agent_ref.is_empty() {
            return ExtensionCommandOutcome::Error("Usage: /skills revoke <bank> <agent>".into());
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
            .revoke_on_skill_bank(&bank.db_id, &agent.pubkey)
            .await
        {
            return ExtensionCommandOutcome::Error(format!("Failed to revoke auth: {e}"));
        }
        let ref_removed = match self
            .registry
            .open_agent_db(&agent.db_id, Some(&agent.pubkey))
            .await
        {
            Ok(Some(db)) => db.detach_skill_bank(&bank.display_name).await.ok(),
            _ => None,
        };
        let mut msg = format!(
            "Revoked agent '{}'s access to skill bank '{}'",
            agent.display_name, bank.display_name
        );
        if ref_removed != Some(true) {
            msg.push_str(
                " (note: couldn't remove the ref from the agent's skill_banks subtree — auth \
                 is revoked regardless)",
            );
        }
        ExtensionCommandOutcome::Text(msg)
    }

    async fn share_cmd(&self, bank_ref: &str) -> ExtensionCommandOutcome {
        if bank_ref.is_empty() {
            return ExtensionCommandOutcome::Error("Usage: /skills share <bank>".into());
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
                "Share this ticket to sync skill bank '{}' (DB {}):\n\n{ticket}",
                entry.display_name, entry.db_id
            )),
            Err(e) => ExtensionCommandOutcome::Error(format!("Failed to share skill bank: {e}")),
        }
    }

    async fn unshare_cmd(&self, bank_ref: &str) -> ExtensionCommandOutcome {
        if bank_ref.is_empty() {
            return ExtensionCommandOutcome::Error("Usage: /skills unshare <bank>".into());
        }
        let entry = match self.resolve_bank(bank_ref) {
            Ok(e) => e,
            Err(msg) => return ExtensionCommandOutcome::Error(msg),
        };
        match self.registry.disable_sync_for(&entry.db_id).await {
            Ok(()) => ExtensionCommandOutcome::Text(format!(
                "Sync disabled for skill bank '{}' — it is no longer shared.",
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
                "Usage: /skills import <ticket> [admin|write|read]".into(),
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
            Ok(SkillImportOutcome::Imported {
                display_name,
                db_id,
            }) => ExtensionCommandOutcome::Text(format!(
                "Imported skill bank '{display_name}' (DB {db_id}). Grant it to agents with \
                 /skills grant {display_name} <agent> <read|write>."
            )),
            Ok(SkillImportOutcome::AlreadyLocal { display_name }) => ExtensionCommandOutcome::Text(
                format!("Skill bank '{display_name}' is already hosted on this peer."),
            ),
            Ok(SkillImportOutcome::Pending { request_id }) => {
                ExtensionCommandOutcome::Text(format!(
                    "Bootstrap request {request_id} pending the owner's approval. Re-run \
                     `/skills import <ticket>` after they run `/sharing approve {request_id}`."
                ))
            }
            Err(msg) => ExtensionCommandOutcome::Error(msg),
        }
    }

    async fn import_bank_via_ticket(
        &self,
        ticket: &eidetica::sync::DatabaseTicket,
        permission: crate::commands::CoOwnerPermission,
    ) -> Result<SkillImportOutcome, String> {
        let db_id = ticket.database_id().clone();
        if let Some(entry) = self.skill_bank_index.find_by_id(&db_id) {
            return Ok(SkillImportOutcome::AlreadyLocal {
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
            }) => return Ok(SkillImportOutcome::Pending { request_id }),
            Err(e) => return Err(format!("Bootstrap failed: {e}")),
        }
        let bank_db = match self.registry.open_skill_bank(&db_id, None).await {
            Ok(Some(db)) => db,
            Ok(None) => {
                return Err(format!(
                    "Bootstrap reported success on skill bank {db_id} but this peer still holds \
                     no key. Re-run the import to retry."
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
                "skill-bank-{}",
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
        self.skill_bank_index
            .register(crate::hosted_index::DbEntry {
                db_id: db_id.clone(),
                display_name: display_name.clone(),
                pubkey,
            });
        if let Err(e) = self.registry.enable_sync_for(&db_id).await {
            return Err(format!(
                "Imported skill bank '{display_name}' (DB {db_id}) but failed to enable ongoing \
                 sync: {e}"
            ));
        }
        Ok(SkillImportOutcome::Imported {
            display_name,
            db_id,
        })
    }

    /// `/skills attach <bank|db_id|ticket>` — attach a skill bank to
    /// the current session. Mirror of `/memory attach`; resolved
    /// display name is what lands in
    /// `extension_settings["skills"]["attached_banks"]`.
    async fn attach_cmd(&self, arg: &str, ctx: &HookContext) -> ExtensionCommandOutcome {
        if arg.is_empty() {
            return ExtensionCommandOutcome::Error(
                "Usage: /skills attach <bank|db_id|ticket>".into(),
            );
        }

        let (bank_name, prelude): (String, Option<String>) =
            if let Ok(ticket) = arg.parse::<eidetica::sync::DatabaseTicket>() {
                match self
                    .import_bank_via_ticket(&ticket, crate::commands::CoOwnerPermission::Write)
                    .await
                {
                    Ok(SkillImportOutcome::Imported {
                        display_name,
                        db_id,
                    }) => (
                        display_name.clone(),
                        Some(format!(
                            "Imported skill bank '{display_name}' (DB {db_id}) via ticket. \
                             Now attaching to this session."
                        )),
                    ),
                    Ok(SkillImportOutcome::AlreadyLocal { display_name }) => (display_name, None),
                    Ok(SkillImportOutcome::Pending { request_id }) => {
                        return ExtensionCommandOutcome::Text(format!(
                            "Bootstrap request {request_id} pending the owner's approval. \
                             Re-run `/skills attach <ticket>` after they run \
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

        let mut settings = ctx.get_settings("skills").await;
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
        match ctx.set_settings("skills", settings).await {
            Ok(()) => ExtensionCommandOutcome::Text(format!(
                "{}Attached skill bank '{bank_name}' to this session. Its skills will be \
                 surfaced in context.",
                prelude.map(|p| format!("{p}\n")).unwrap_or_default()
            )),
            Err(e) => ExtensionCommandOutcome::Error(format!("Failed to persist: {e}")),
        }
    }
}

async fn detach_cmd(bank_name: &str, ctx: &HookContext) -> ExtensionCommandOutcome {
    if bank_name.is_empty() {
        return ExtensionCommandOutcome::Error("Usage: /skills detach <bank_name>".into());
    }
    let mut settings = ctx.get_settings("skills").await;
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
    match ctx.set_settings("skills", settings).await {
        Ok(()) => ExtensionCommandOutcome::Text(format!(
            "Detached skill bank '{bank_name}' from this session."
        )),
        Err(e) => ExtensionCommandOutcome::Error(format!("Failed to persist: {e}")),
    }
}

/// Result of [`SkillCommand::import_bank_via_ticket`]. Lets the caller
/// (`/skills import` or the ticket-aware `/skills attach`) render
/// appropriate messaging.
enum SkillImportOutcome {
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
