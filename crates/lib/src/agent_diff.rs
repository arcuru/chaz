//! Explicit Agent yaml↔DB comparison and merging.
//!
//! An Agent's declarative shape lives in two places: the **yaml** template
//! (`AgentConfig`, which seeds a new Agent DB) and the **DB-actual** config
//! ([`AgentDbConfig`], authoritative once the agent exists). The two can
//! differ — a live `/agent set` edit changes the DB without touching yaml, and
//! a yaml edit is never applied to an existing agent automatically.
//!
//! This module is the pure, testable core behind the TUI Peer→Agents diff
//! view: it computes a field-level diff between the two and applies an
//! explicitly chosen merge back into the DB config. It does no IO — the server
//! layer reads/writes the actual DBs and resolves the prompt blob.
//!
//! ## Merge modes
//! Both modes are user actions; nothing calls them on its own.
//! - [`AgentMergeMode::Reseed`] (`[R]`, confirmed) — copy every declarative
//!   field, including worker templates, from yaml into the DB.
//! - [`AgentMergeMode::Pick`] (`[a]`, "pick changes") — apply exactly the
//!   user-selected fields.
//!
//! There is deliberately no "fill in what the DB never set" mode: a value equal
//! to the type default may be a deliberate `/agent set` choice (e.g.
//! `autonomous=false`), and the DB keeps no record of which fields were set.

use crate::agent_db::AgentDbConfig;

/// How a single field (or worker) differs between the yaml-declared config and
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
/// `system_prompt_ref` and `applied_config_hash` are runtime bookkeeping, not
/// declarative, so they're excluded — the server re-resolves the prompt
/// pointer when a prompt field is merged. `default_memory_banks` and
/// `default_skill_banks` are excluded too: bank attachment reads them from
/// yaml at startup, not from the DB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentField {
    SystemPrompt,
    SystemPromptFiles,
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
    /// Every mergeable field, in display order.
    pub const ALL: &'static [Self] = &[
        Self::SystemPrompt,
        Self::SystemPromptFiles,
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

    /// Copy this field's value from `from` into `into`. The single per-field
    /// write primitive behind every merge mode.
    pub fn copy_into(self, from: &AgentDbConfig, into: &mut AgentDbConfig) {
        match self {
            Self::SystemPrompt => into.system_prompt = from.system_prompt.clone(),
            Self::SystemPromptFiles => into.system_prompt_files = from.system_prompt_files.clone(),
            Self::Model => into.model = from.model.clone(),
            Self::Tools => into.tools = from.tools.clone(),
            Self::MaxSpawnDepth => into.max_spawn_depth = from.max_spawn_depth,
            Self::Autonomous => into.autonomous = from.autonomous,
            Self::Presets => into.presets = from.presets.clone(),
            Self::ToolProfile => into.tool_profile = from.tool_profile.clone(),
            Self::MaxContextTokens => into.max_context_tokens = from.max_context_tokens,
            Self::Capabilities => into.capabilities = from.capabilities.clone(),
            Self::Grants => into.grants = from.grants.clone(),
            Self::Workers => into.workers = from.workers.clone(),
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
/// field row. Merging applies the whole template list via that row.
#[derive(Clone, Debug)]
pub struct WorkerDiffRow {
    pub name: String,
    pub status: FieldStatus,
}

/// Full diff between a yaml-declared config and a DB-actual config.
#[derive(Clone, Debug)]
pub struct AgentDiff {
    pub rows: Vec<AgentDiffRow>,
    pub workers: Vec<WorkerDiffRow>,
}

impl AgentDiff {
    /// Whether any mergeable field differs, including worker templates.
    /// Used by the TUI to short-circuit "nothing to merge".
    pub fn has_field_changes(&self) -> bool {
        self.rows.iter().any(|r| r.status != FieldStatus::Unchanged)
    }
}

/// Compute the field-level diff. `yaml` is the yaml-derived config
/// (`AgentDbConfig::from_agent_config(...)`); `db` is the DB-actual config.
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

/// Which merge to apply. See module docs.
#[derive(Clone, Debug)]
pub enum AgentMergeMode {
    /// `[R]` (confirmed) — copy every declarative field from yaml.
    Reseed,
    /// `[a]` — apply exactly these fields.
    Pick(Vec<AgentField>),
}

impl AgentMergeMode {
    /// The concrete field set this mode writes.
    pub fn fields(&self) -> Vec<AgentField> {
        match self {
            AgentMergeMode::Reseed => AgentField::ALL.to_vec(),
            AgentMergeMode::Pick(fields) => fields.clone(),
        }
    }
}

/// Apply a merge: returns a new DB config with the selected fields taken from
/// `yaml` and everything else (including workers and runtime bookkeeping)
/// preserved from `db`. Pure — the caller persists the result.
pub fn apply_merge(
    db: &AgentDbConfig,
    yaml: &AgentDbConfig,
    mode: &AgentMergeMode,
) -> AgentDbConfig {
    let fields = mode.fields();
    let mut out = db.clone();
    for f in &fields {
        f.copy_into(yaml, &mut out);
    }
    out
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
    fn reseed_overwrites_everything_from_yaml() {
        let mut yaml = cfg();
        yaml.model = Some("opus".into());
        yaml.max_spawn_depth = Some(20);

        let mut db = cfg();
        db.model = Some("haiku".into());
        db.max_spawn_depth = Some(99);

        let merged = apply_merge(&db, &yaml, &AgentMergeMode::Reseed);
        assert_eq!(merged.model.as_deref(), Some("opus"));
        assert_eq!(merged.max_spawn_depth, Some(20));
    }

    #[test]
    fn reseed_copies_workers_and_preserves_runtime_bookkeeping() {
        let mut yaml = cfg();
        yaml.workers = vec![WorkerDbConfig {
            name: "writer".into(),
            ..Default::default()
        }];
        let mut db = cfg();
        db.workers = vec![WorkerDbConfig {
            name: "researcher".into(),
            ..Default::default()
        }];
        db.applied_config_hash = Some("deadbeef".into());
        db.default_memory_banks = vec!["notes".into()];

        let merged = apply_merge(&db, &yaml, &AgentMergeMode::Reseed);
        assert_eq!(merged.workers, yaml.workers);
        assert_eq!(merged.applied_config_hash.as_deref(), Some("deadbeef"));
        assert_eq!(merged.default_memory_banks, vec!["notes".to_string()]);
    }

    #[test]
    fn pick_leaves_deliberate_default_values_alone() {
        // `autonomous=false` and empty tools may be deliberate `/agent set`
        // choices. Only a picked field changes; nothing is inferred from a
        // value equalling the default.
        let mut yaml = cfg();
        yaml.autonomous = true;
        yaml.tools = Some(vec!["shell".into()]);
        yaml.model = Some("opus".into());
        let mut db = cfg();
        db.tools = Some(vec![]);

        let merged = apply_merge(&db, &yaml, &AgentMergeMode::Pick(vec![AgentField::Model]));
        assert_eq!(merged.model.as_deref(), Some("opus"));
        assert!(!merged.autonomous);
        assert_eq!(merged.tools, Some(vec![]));
    }

    #[test]
    fn pick_applies_only_selected_fields() {
        let mut yaml = cfg();
        yaml.model = Some("opus".into());
        yaml.tool_profile = Some("deep".into());

        let db = cfg();
        let merged = apply_merge(&db, &yaml, &AgentMergeMode::Pick(vec![AgentField::Model]));
        assert_eq!(merged.model.as_deref(), Some("opus"));
        // tool_profile not picked → stays at db default.
        assert_eq!(merged.tool_profile, None);
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
    fn reseed_fields_cover_every_declarative_field() {
        assert_eq!(AgentMergeMode::Reseed.fields(), AgentField::ALL.to_vec());
        assert!(AgentField::ALL.contains(&AgentField::Workers));
    }
}
