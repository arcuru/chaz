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
    CapProvider, CapabilityKind, CapabilityRequest, ExtensionCaps, PromptAugmentation,
};
use crate::extension::handler::InstalledExtension;
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
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

    /// Find skills whose triggers match any word in the recent message text.
    pub fn match_and_assemble(&self, recent_message_text: &[String]) -> Option<String> {
        let words: Vec<String> = recent_message_text
            .iter()
            .flat_map(|msg| {
                msg.split(|c: char| !c.is_alphanumeric())
                    .map(|w| w.to_lowercase())
                    .filter(|w| !w.is_empty() && !is_stopword(w))
                    .collect::<Vec<_>>()
            })
            .collect();

        let mut parts: Vec<String> = Vec::new();
        for skill in &self.skills {
            let matched = skill
                .triggers
                .iter()
                .any(|t| words.iter().any(|w| w == t.as_str()));
            if matched {
                parts.push(format!(
                    "<skill name=\"{}\">\n{}\n</skill>",
                    skill.name, skill.body
                ));
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
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

fn is_stopword(word: &str) -> bool {
    matches!(
        word,
        "the"
            | "a"
            | "an"
            | "is"
            | "are"
            | "was"
            | "were"
            | "be"
            | "been"
            | "in"
            | "on"
            | "at"
            | "to"
            | "for"
            | "of"
            | "with"
            | "and"
            | "or"
            | "but"
            | "not"
            | "it"
            | "this"
            | "that"
            | "i"
            | "you"
            | "he"
            | "she"
            | "we"
            | "they"
            | "do"
            | "does"
            | "did"
            | "can"
            | "will"
            | "would"
            | "what"
            | "how"
            | "when"
            | "where"
            | "which"
            | "who"
            | "my"
            | "your"
            | "our"
            | "their"
            | "just"
            | "also"
            | "only"
            | "now"
            | "then"
            | "here"
            | "so"
            | "if"
            | "no"
            | "yes"
            | "ok"
            | "okay"
            | "please"
            | "thanks"
            | "thank"
    )
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
    registry: Arc<std::sync::RwLock<SkillRegistry>>,
}

impl SkillsExtension {
    pub fn new() -> Self {
        Self {
            registry: Arc::new(std::sync::RwLock::new(SkillRegistry::new())),
        }
    }
}

impl Extension for SkillsExtension {
    fn name(&self) -> &'static str {
        "skills"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![],
            required_capabilities: vec![CapabilityRequest::ToolRegistration],
            requested_capabilities: Vec::new(),
            provides_capabilities: vec![CapabilityKind::PromptAugmentation],
        }
    }

    fn install<'a>(
        &'a self,
        caps: ExtensionCaps,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<InstalledExtension>> + Send + 'a>> {
        let skill_count = {
            let mut registry = self.registry.write().unwrap();
            registry.scan();
            registry.list().len()
        };

        Box::pin(async move {
            let tool_reg = caps
                .tool_registration
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("skills install requires ToolRegistration cap"))?;

            let tools: Vec<Arc<dyn Tool>> = vec![
                Arc::new(SkillListTool {
                    registry: self.registry.clone(),
                }),
                Arc::new(SkillSearchTool {
                    registry: self.registry.clone(),
                }),
                Arc::new(SkillShowTool {
                    registry: self.registry.clone(),
                }),
            ];
            for t in &tools {
                let d = t.descriptor();
                tool_reg.register(d, t.clone()).await?;
            }

            tracing::info!(count = skill_count, "Skills extension installed");
            Ok(InstalledExtension::empty())
        })
    }

    fn build_providers(&self) -> anyhow::Result<HashMap<CapabilityKind, CapProvider>> {
        let pa: Arc<dyn PromptAugmentation> = Arc::new(SkillsPromptAugmentation {
            registry: self.registry.clone(),
        });
        Ok([(
            CapabilityKind::PromptAugmentation,
            CapProvider::PromptAugmentation(pa),
        )]
        .into_iter()
        .collect())
    }
}

// ── PromptAugmentation impl ─────────────────────────────────────────

struct SkillsPromptAugmentation {
    registry: Arc<std::sync::RwLock<SkillRegistry>>,
}

impl PromptAugmentation for SkillsPromptAugmentation {
    fn augment_system_prompt<'a>(
        &'a self,
        _agent_name: &'a str,
        recent_message_text: &'a [String],
    ) -> crate::extension::caps::CapFuture<'a, Option<String>> {
        Box::pin(async move {
            let registry = self.registry.read().unwrap();
            Ok(registry.match_and_assemble(recent_message_text))
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

struct SkillShowTool {
    registry: Arc<std::sync::RwLock<SkillRegistry>>,
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
        _ctx: &'a crate::tool::ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        let name = arguments["name"].as_str().unwrap_or("").to_string();
        let found: Option<String> = {
            let registry = self.registry.read().unwrap();
            registry.list().iter().find(|s| s.name == name).map(|s| {
                serde_json::json!({
                    "text": s.body,
                    "name": s.name,
                    "description": s.description,
                    "triggers": &s.triggers,
                })
                .to_string()
            })
        };
        Box::pin(async move {
            match found {
                Some(json) => Ok(json),
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
