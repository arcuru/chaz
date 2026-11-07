//! Read-only pending-folder versus accepted Agent DB comparison.
//! Field-default equality labels display differences; it never chooses which
//! values survive a reload. Imports replace the complete editable scope.

use crate::agent_db::AgentDbConfig;

/// How a single field (or worker) differs between the folder draft and
/// the DB-actual config. Direction is yaml→DB: "added" means applying yaml
/// would *add* a value the DB lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldStatus {
    /// Both sides equal.
    Unchanged,
    /// yaml declares a value, the DB is at its default — yaml would add it.
    Added,
    /// The DB has a value, yaml does not — yaml would remove it.
    Removed,
    /// Both declare a value and they differ.
    Changed,
}

/// The declarative `AgentDbConfig` fields the diff/merge covers.
/// `system_prompt_ref` and `applied_config_hash` are legacy bookkeeping, not
/// editable settings. Individual prompt contents have their own display row. `default_memory_banks` and
/// `default_skill_banks` are excluded too: bank attachment reads them from
/// yaml at startup, not from the DB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentField {
    SystemPrompt,
    SystemPromptFiles,
    PromptSources,
    Model,
    Tools,
    MaxSpawnDepth,
    Autonomous,
    Presets,
    ToolProfile,
    MaxContextTokens,
    Capabilities,
    Grants,
    Workers,
}

impl AgentField {
    /// Every displayed field, in display order.
    pub const ALL: &'static [Self] = &[
        Self::SystemPrompt,
        Self::SystemPromptFiles,
        Self::PromptSources,
        Self::Model,
        Self::Tools,
        Self::MaxSpawnDepth,
        Self::Autonomous,
        Self::Presets,
        Self::ToolProfile,
        Self::MaxContextTokens,
        Self::Capabilities,
        Self::Grants,
        Self::Workers,
    ];

    /// Human label shown in the diff view.
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemPrompt => "system_prompt",
            Self::SystemPromptFiles => "system_prompt_files",
            Self::PromptSources => "prompt_contents",
            Self::Model => "default_model",
            Self::Tools => "allowed_tools",
            Self::MaxSpawnDepth => "max_spawn_depth",
            Self::Autonomous => "autonomous",
            Self::Presets => "presets",
            Self::ToolProfile => "tool_profile",
            Self::MaxContextTokens => "max_context_tokens",
            Self::Capabilities => "capabilities",
            Self::Grants => "grants",
            Self::Workers => "workers",
        }
    }

    /// Whether `cfg`'s value for this field is the type default. Display-only:
    /// it labels a diff row Added/Removed. It says nothing about whether a user
    /// chose that value, so no merge decision may rest on it.
    pub fn is_default(self, cfg: &AgentDbConfig) -> bool {
        let d = AgentDbConfig::default();
        self.eq_field(cfg, &d)
    }

    /// Structural equality of just this field between two configs. Used for the
    /// diff status (separate from [`Self::render`], whose previews could collide).
    pub fn eq_field(self, a: &AgentDbConfig, b: &AgentDbConfig) -> bool {
        match self {
            Self::SystemPrompt => a.system_prompt == b.system_prompt,
            Self::SystemPromptFiles => a.system_prompt_files == b.system_prompt_files,
            Self::PromptSources => a.prompt_sources == b.prompt_sources,
            Self::Model => a.model == b.model,
            Self::Tools => a.tools == b.tools,
            Self::MaxSpawnDepth => a.max_spawn_depth == b.max_spawn_depth,
            Self::Autonomous => a.autonomous == b.autonomous,
            Self::Presets => a.presets == b.presets,
            Self::ToolProfile => a.tool_profile == b.tool_profile,
            Self::MaxContextTokens => a.max_context_tokens == b.max_context_tokens,
            Self::Capabilities => a.capabilities == b.capabilities,
            Self::Grants => a.grants == b.grants,
            Self::Workers => a.workers == b.workers,
        }
    }

    /// Display string for this field's value. May be a one-line preview for
    /// long text (system prompt) — never used for equality.
    pub fn render(self, cfg: &AgentDbConfig) -> String {
        match self {
            Self::SystemPrompt => preview(&cfg.system_prompt),
            Self::SystemPromptFiles => {
                if cfg.system_prompt_files.is_empty() {
                    "(none)".to_string()
                } else {
                    cfg.system_prompt_files.join(", ")
                }
            }
            Self::PromptSources => cfg
                .prompt_sources
                .as_ref()
                .map(|sources| {
                    sources
                        .files
                        .iter()
                        .map(|(path, text)| {
                            let hash = blake3::hash(text.as_bytes()).to_hex();
                            format!("{path} [{}]: {}", &hash[..8], preview(text))
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_else(|| "(legacy sources)".into()),
            Self::Model => opt_str(cfg.model.as_deref()),
            Self::Tools => render_tools(cfg.tools.as_deref()),
            Self::MaxSpawnDepth => cfg
                .max_spawn_depth
                .map(|n| n.to_string())
                .unwrap_or_else(|| "(default 10)".to_string()),
            Self::Autonomous => if cfg.autonomous { "yes" } else { "no" }.to_string(),
            Self::Presets => {
                if cfg.presets.is_empty() {
                    "(none)".to_string()
                } else {
                    let mut names: Vec<&str> = cfg.presets.keys().map(String::as_str).collect();
                    names.sort_unstable();
                    names.join(", ")
                }
            }
            Self::ToolProfile => opt_str(cfg.tool_profile.as_deref()),
            Self::MaxContextTokens => cfg
                .max_context_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "(default)".to_string()),
            Self::Capabilities => {
                if cfg.capabilities == Default::default() {
                    "(none)".to_string()
                } else {
                    json_compact(&cfg.capabilities)
                }
            }
            Self::Grants => {
                if cfg.grants.is_empty() {
                    "(none)".to_string()
                } else {
                    json_compact(&cfg.grants)
                }
            }
            Self::Workers => {
                if cfg.workers.is_empty() {
                    "(none)".to_string()
                } else {
                    let mut names: Vec<&str> =
                        cfg.workers.iter().map(|w| w.name.as_str()).collect();
                    names.sort_unstable();
                    names.join(", ")
                }
            }
        }
    }
}

/// One row of the diff — a single field, both rendered values, and the status.
#[derive(Clone, Debug)]
pub struct AgentDiffRow {
    pub field: AgentField,
    pub label: &'static str,
    pub yaml: String,
    pub db: String,
    pub status: FieldStatus,
}

/// One worker entry in the diff: per-template detail behind the `workers`
/// field row. The list is included in a whole-scope reload.
#[derive(Clone, Debug)]
pub struct WorkerDiffRow {
    pub name: String,
    pub status: FieldStatus,
}

/// Full diff between a folder draft and a DB-actual config.
#[derive(Clone, Debug)]
pub struct AgentDiff {
    pub rows: Vec<AgentDiffRow>,
    pub workers: Vec<WorkerDiffRow>,
}

impl AgentDiff {
    /// Whether any displayed field differs, including sources and workers.
    /// Used by read-only administration displays.
    pub fn has_field_changes(&self) -> bool {
        self.rows.iter().any(|r| r.status != FieldStatus::Unchanged)
    }
}

/// Compute the field-level diff. `yaml` is the validated folder config; `db` is the DB-actual config.
pub fn diff_agent(yaml: &AgentDbConfig, db: &AgentDbConfig) -> AgentDiff {
    let rows = AgentField::ALL
        .iter()
        .map(|&field| {
            let same = field.eq_field(yaml, db);
            let yaml_default = field.is_default(yaml);
            let db_default = field.is_default(db);
            let status = if same {
                FieldStatus::Unchanged
            } else if yaml_default {
                FieldStatus::Removed
            } else if db_default {
                FieldStatus::Added
            } else {
                FieldStatus::Changed
            };
            AgentDiffRow {
                field,
                label: field.label(),
                yaml: field.render(yaml),
                db: field.render(db),
                status,
            }
        })
        .collect();

    AgentDiff {
        rows,
        workers: diff_workers(yaml, db),
    }
}

/// Diff worker templates by name. Display-only.
fn diff_workers(yaml: &AgentDbConfig, db: &AgentDbConfig) -> Vec<WorkerDiffRow> {
    let mut names: Vec<String> = yaml
        .workers
        .iter()
        .map(|w| w.name.clone())
        .chain(db.workers.iter().map(|w| w.name.clone()))
        .collect();
    names.sort_unstable();
    names.dedup();

    names
        .into_iter()
        .map(|name| {
            let y = yaml.workers.iter().find(|w| w.name == name);
            let d = db.workers.iter().find(|w| w.name == name);
            let status = match (y, d) {
                (Some(yw), Some(dw)) if yw == dw => FieldStatus::Unchanged,
                (Some(_), Some(_)) => FieldStatus::Changed,
                (Some(_), None) => FieldStatus::Added,
                (None, Some(_)) => FieldStatus::Removed,
                (None, None) => FieldStatus::Unchanged, // unreachable (name came from one)
            };
            WorkerDiffRow { name, status }
        })
        .collect()
}

// ---- small render helpers ------------------------------------------------

fn opt_str(v: Option<&str>) -> String {
    v.map(|s| s.to_string())
        .unwrap_or_else(|| "(unset)".to_string())
}

fn render_tools(tools: Option<&[String]>) -> String {
    match tools {
        None => "all".to_string(),
        Some([]) => "(none)".to_string(),
        Some(v) => v.join(", "),
    }
}

/// First non-empty line of `s`, ellipsized; `(empty)` when blank.
fn preview(s: &str) -> String {
    if s.trim().is_empty() {
        return "(empty)".to_string();
    }
    let first = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    const MAX: usize = 80;
    if first.chars().count() > MAX {
        let truncated: String = first.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    } else {
        first.to_string()
    }
}

fn json_compact<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "(unserializable)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_db::{AgentDbConfig, WorkerDbConfig};

    fn cfg() -> AgentDbConfig {
        AgentDbConfig::default()
    }

    #[test]
    fn unchanged_when_identical() {
        let mut a = cfg();
        a.model = Some("opus".into());
        let diff = diff_agent(&a, &a);
        assert!(diff.rows.iter().all(|r| r.status == FieldStatus::Unchanged));
        assert!(!diff.has_field_changes());
    }

    #[test]
    fn added_when_yaml_sets_db_default() {
        let mut yaml = cfg();
        yaml.model = Some("opus".into());
        let db = cfg();
        let diff = diff_agent(&yaml, &db);
        let row = diff
            .rows
            .iter()
            .find(|r| r.field == AgentField::Model)
            .unwrap();
        assert_eq!(row.status, FieldStatus::Added);
        assert!(diff.has_field_changes());
    }

    #[test]
    fn removed_when_db_set_yaml_default() {
        let yaml = cfg();
        let mut db = cfg();
        db.model = Some("haiku".into());
        let diff = diff_agent(&yaml, &db);
        let row = diff
            .rows
            .iter()
            .find(|r| r.field == AgentField::Model)
            .unwrap();
        assert_eq!(row.status, FieldStatus::Removed);
    }

    #[test]
    fn changed_when_both_set_differently() {
        let mut yaml = cfg();
        yaml.max_spawn_depth = Some(10);
        let mut db = cfg();
        db.max_spawn_depth = Some(40);
        let diff = diff_agent(&yaml, &db);
        let row = diff
            .rows
            .iter()
            .find(|r| r.field == AgentField::MaxSpawnDepth)
            .unwrap();
        assert_eq!(row.status, FieldStatus::Changed);
    }

    #[test]
    fn worker_diff_classifies_presence() {
        let mut yaml = cfg();
        yaml.workers = vec![
            WorkerDbConfig {
                name: "shared".into(),
                ..Default::default()
            },
            WorkerDbConfig {
                name: "yaml-only".into(),
                ..Default::default()
            },
        ];
        let mut db = cfg();
        db.workers = vec![
            WorkerDbConfig {
                name: "shared".into(),
                ..Default::default()
            },
            WorkerDbConfig {
                name: "db-only".into(),
                ..Default::default()
            },
        ];
        let diff = diff_agent(&yaml, &db);
        let find = |n: &str| diff.workers.iter().find(|w| w.name == n).unwrap().status;
        assert_eq!(find("shared"), FieldStatus::Unchanged);
        assert_eq!(find("yaml-only"), FieldStatus::Added);
        assert_eq!(find("db-only"), FieldStatus::Removed);
    }

    #[test]
    fn content_only_prompt_changes_are_visible() {
        let mut accepted = cfg();
        accepted.prompt_sources = Some(crate::agent_bundle::PromptSources {
            version: 1,
            files: std::collections::BTreeMap::from([(
                "prompts/system.md".into(),
                "ACCEPTED".into(),
            )]),
            ..Default::default()
        });
        let mut draft = accepted.clone();
        draft
            .prompt_sources
            .as_mut()
            .unwrap()
            .files
            .insert("prompts/system.md".into(), "EDITED".into());
        let diff = diff_agent(&draft, &accepted);
        let row = diff
            .rows
            .iter()
            .find(|r| r.field == AgentField::PromptSources)
            .unwrap();
        assert_eq!(row.status, FieldStatus::Changed);
        assert!(row.yaml.contains("EDITED"));
        assert!(row.db.contains("ACCEPTED"));
    }
}
