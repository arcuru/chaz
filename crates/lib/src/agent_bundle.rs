//! Portable, coherent agent settings and explicitly referenced prompt sources.
//!
//! The source bundle travels inside the config blob, so a snapshot cannot pair
//! settings from one transaction with source text from another. Filesystem
//! import and projection are operator interfaces, never runtime prompt inputs.

use crate::agent_db::{AgentDbConfig, WorkerDbConfig};
use crate::config::AgentPreset;
use crate::grants::Grants;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct PromptSources {
    pub version: u32,
    pub files: BTreeMap<String, String>,
    /// A flattened legacy snapshot may end in significant inline whitespace.
    /// These sources preserve that exact saved text rather than trimming it.
    /// Only migration creates these markers; ordinary imports use the existing
    /// files-first, trim-file-end, inline-last assembly semantics.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub verbatim: BTreeSet<String>,
}

/// The complete editable scope. Bookkeeping and resource attachments are not
/// part of this document and cannot be reset by omission or smuggled in as keys.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentDocument {
    pub version: u32,
    pub agent_id: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub system_prompt_files: Vec<String>,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub workers: Vec<WorkerDbConfig>,
    pub max_spawn_depth: Option<u32>,
    #[serde(default)]
    pub autonomous: bool,
    #[serde(default)]
    pub presets: HashMap<String, AgentPreset>,
    pub tool_profile: Option<String>,
    pub max_context_tokens: Option<usize>,
    #[serde(default)]
    pub capabilities: Grants,
    #[serde(default)]
    pub grants: HashMap<String, Grants>,
}

impl AgentDocument {
    pub fn from_config(agent_id: &str, cfg: &AgentDbConfig) -> Self {
        Self {
            version: VERSION,
            agent_id: agent_id.into(),
            system_prompt: cfg.system_prompt.clone(),
            system_prompt_files: cfg.system_prompt_files.clone(),
            model: cfg.model.clone(),
            tools: cfg.tools.clone(),
            workers: cfg.workers.clone(),
            max_spawn_depth: cfg.max_spawn_depth,
            autonomous: cfg.autonomous,
            presets: cfg.presets.clone(),
            tool_profile: cfg.tool_profile.clone(),
            max_context_tokens: cfg.max_context_tokens,
            capabilities: cfg.capabilities.clone(),
            grants: cfg.grants.clone(),
        }
    }

    /// Parse through Value first: duplicate mapping keys are rejected even
    /// inside user-named maps (presets/grants), not just on typed struct fields.
    pub fn parse(yaml: &str, agent_id: &str) -> anyhow::Result<Self> {
        let value: serde_yaml::Value = serde_yaml::from_str(yaml)?;
        validate_nested_keys(&value)?;
        let doc: Self = serde_yaml::from_value(value)?;
        ensure!(
            doc.version == VERSION,
            "unsupported agent document version {}",
            doc.version
        );
        ensure!(doc.agent_id == agent_id, "agent identity mismatch");
        let mut names = BTreeSet::new();
        for worker in &doc.workers {
            ensure!(
                !worker.name.is_empty() && names.insert(&worker.name),
                "empty or duplicate worker name"
            );
        }
        for path in doc.paths() {
            validate_source_path(path)?;
        }
        Ok(doc)
    }

    fn paths(&self) -> impl Iterator<Item = &str> {
        self.system_prompt_files.iter().map(String::as_str).chain(
            self.workers
                .iter()
                .flat_map(|w| w.system_prompt_files.iter().map(String::as_str)),
        )
    }

    pub fn replace_scope(
        &self,
        old: &AgentDbConfig,
        sources: PromptSources,
    ) -> anyhow::Result<AgentDbConfig> {
        let mut cfg = old.clone();
        cfg.system_prompt = self.system_prompt.clone();
        cfg.system_prompt_files = self.system_prompt_files.clone();
        cfg.model = self.model.clone();
        cfg.tools = self.tools.clone();
        cfg.workers = self.workers.clone();
        cfg.max_spawn_depth = self.max_spawn_depth;
        cfg.autonomous = self.autonomous;
        cfg.presets = self.presets.clone();
        cfg.tool_profile = self.tool_profile.clone();
        cfg.max_context_tokens = self.max_context_tokens;
        cfg.capabilities = self.capabilities.clone();
        cfg.grants = self.grants.clone();
        cfg.prompt_sources = Some(sources);
        // Historical peer-local snapshots remain in their store, but no new
        // config depends on another peer's unsynchronized prompt pointer.
        cfg.system_prompt_ref = None;
        validate(&cfg)?;
        Ok(cfg)
    }

    pub fn read_sources(
        &self,
        folder: &Path,
        previous: Option<&PromptSources>,
    ) -> anyhow::Result<PromptSources> {
        let mut sources = PromptSources {
            version: VERSION,
            ..Default::default()
        };
        for path in self.paths() {
            let file = source_path(folder, path)?;
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            if let Some(old) = previous
                && old.verbatim.contains(path)
                && old.files.get(path) == Some(&text)
            {
                sources.verbatim.insert(path.into());
            }
            sources.files.insert(path.into(), text);
        }
        Ok(sources)
    }
}

pub fn validate_source_path(path: &str) -> anyhow::Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains('\\')
            && path.split('/').all(|p| !matches!(p, "" | "." | "..")),
        "invalid prompt path {path:?}"
    );
    let parts: Vec<_> = Path::new(path).components().collect();
    ensure!(
        parts.iter().all(|c| matches!(c, Component::Normal(_))),
        "prompt path must be relative and normalized: {path}"
    );
    ensure!(
        parts
            .first()
            .is_some_and(|c| c.as_os_str() != "agent.yaml"
                && !c.as_os_str().to_string_lossy().starts_with('.')),
        "reserved prompt path: {path}"
    );
    Ok(())
}

/// Refuse symlinks and special files at every level. External sources are
/// imported separately; managed sources must be regular files in this folder.
pub fn source_path(folder: &Path, relative: &str) -> anyhow::Result<std::path::PathBuf> {
    validate_source_path(relative)?;
    let mut path = folder.to_path_buf();
    crate::agent_folder::checked_dir(&path, false)?;
    let components: Vec<_> = Path::new(relative).components().collect();
    for (i, part) in components.iter().enumerate() {
        path.push(part);
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect {}", path.display()))?;
        ensure!(
            !meta.file_type().is_symlink(),
            "symlink prompt path: {}",
            path.display()
        );
        ensure!(
            if i + 1 == components.len() {
                meta.is_file()
            } else {
                meta.is_dir()
            },
            "not a regular prompt path: {}",
            path.display()
        );
    }
    Ok(path)
}

pub fn assemble(inline: &str, paths: &[String], sources: &PromptSources) -> anyhow::Result<String> {
    let mut parts = Vec::new();
    for path in paths {
        let text = sources
            .files
            .get(path)
            .with_context(|| format!("missing synchronized prompt source {path}"))?;
        let text = if sources.verbatim.contains(path) {
            text.as_str()
        } else {
            text.trim_end()
        };
        if !text.is_empty() {
            parts.push(text);
        }
    }
    if !inline.is_empty() {
        parts.push(inline);
    }
    Ok(parts.join("\n\n"))
}

pub fn validate(cfg: &AgentDbConfig) -> anyhow::Result<()> {
    let Some(sources) = &cfg.prompt_sources else {
        return Ok(());
    };
    ensure!(
        sources.version == VERSION,
        "unsupported prompt source version {}",
        sources.version
    );
    let doc = AgentDocument::from_config("", cfg);
    let referenced: BTreeSet<_> = doc.paths().collect();
    ensure!(
        referenced == sources.files.keys().map(String::as_str).collect(),
        "source bundle must contain exactly the referenced files"
    );
    for path in &referenced {
        validate_source_path(path)?;
    }
    ensure!(
        sources
            .verbatim
            .iter()
            .all(|p| sources.files.contains_key(p)),
        "unknown verbatim source"
    );
    for path in &referenced {
        ensure!(
            !referenced
                .iter()
                .any(|other| other != path && Path::new(other).starts_with(path)),
            "prompt file/directory collision: {path}"
        );
    }
    assemble(&cfg.system_prompt, &cfg.system_prompt_files, sources)?;
    for worker in &cfg.workers {
        assemble(&worker.system_prompt, &worker.system_prompt_files, sources)?;
    }
    Ok(())
}

/// One-shot external import. The original files (including Nix symlink
/// targets) are read, never written, and never retained as write-back paths.
pub fn import_sources(cfg: &mut AgentDbConfig) -> anyhow::Result<()> {
    let mut sources = PromptSources {
        version: VERSION,
        ..Default::default()
    };
    import_list(&mut cfg.system_prompt_files, "parent", &mut sources)?;
    for (i, worker) in cfg.workers.iter_mut().enumerate() {
        import_list(
            &mut worker.system_prompt_files,
            &format!("worker-{i}"),
            &mut sources,
        )?;
    }
    cfg.prompt_sources = Some(sources);
    cfg.system_prompt_ref = None;
    validate(cfg)
}

/// An explicit parent source-list change imports only that list. Stored worker
/// fragments and all non-prompt settings remain independent of pending files.
pub fn import_parent_sources(cfg: &mut AgentDbConfig, folder: Option<&Path>) -> anyhow::Result<()> {
    let mut sources = cfg
        .prompt_sources
        .clone()
        .context("missing portable source bundle")?;
    for (i, path) in cfg.system_prompt_files.iter_mut().enumerate() {
        let managed = folder
            .filter(|_| validate_source_path(path).is_ok())
            .map(|f| f.join(&*path));
        let external = match managed {
            Some(file) if file.try_exists()? => source_path(folder.unwrap(), path)?,
            _ => crate::agent::expand_home(Path::new(path)),
        };
        ensure!(
            std::fs::metadata(&external)?.is_file(),
            "not a regular prompt source: {}",
            external.display()
        );
        let text = std::fs::read_to_string(&external)
            .with_context(|| format!("read {}", external.display()))?;
        let hash = blake3::hash(text.as_bytes()).to_hex();
        *path = format!("prompts/import-{i}-{}.md", &hash[..16]);
        sources.files.insert(path.clone(), text);
    }
    let doc = AgentDocument::from_config("", cfg);
    let referenced: BTreeSet<_> = doc.paths().collect();
    sources
        .files
        .retain(|path, _| referenced.contains(path.as_str()));
    sources
        .verbatim
        .retain(|path| referenced.contains(path.as_str()));
    cfg.prompt_sources = Some(sources);
    cfg.system_prompt_ref = None;
    validate(cfg)
}

fn import_list(
    paths: &mut [String],
    prefix: &str,
    sources: &mut PromptSources,
) -> anyhow::Result<()> {
    for (i, path) in paths.iter_mut().enumerate() {
        let external = crate::agent::expand_home(Path::new(path));
        ensure!(
            std::fs::metadata(&external)?.is_file(),
            "not a regular source: {}",
            external.display()
        );
        let text = std::fs::read_to_string(&external)
            .with_context(|| format!("read {}", external.display()))?;
        *path = format!("prompts/{prefix}-{i}.md");
        sources.files.insert(path.clone(), text);
    }
    Ok(())
}

/// Preserve the saved effective parent prompt, not current edits to external
/// files. Workers have no legacy snapshot: unreadable worker sources are a
/// migration blocker rather than a silently truncated prompt.
pub fn migrate(cfg: &AgentDbConfig, saved_parent: &str) -> anyhow::Result<(AgentDbConfig, bool)> {
    if cfg.prompt_sources.is_some() {
        validate(cfg)?;
        return Ok((cfg.clone(), false));
    }
    let mut imported = cfg.clone();
    // Parent source failures can be recovered from its saved snapshot. Import
    // workers independently so their failures cannot be hidden by flattening.
    let parent_paths = std::mem::take(&mut imported.system_prompt_files);
    import_sources(&mut imported)?;
    let mut parent = PromptSources {
        version: VERSION,
        ..Default::default()
    };
    let mut paths = parent_paths;
    let copied = import_list(&mut paths, "parent", &mut parent).is_ok()
        && assemble(&cfg.system_prompt, &paths, &parent)? == saved_parent;
    let sources = imported.prompt_sources.as_mut().expect("imported sources");
    if copied {
        imported.system_prompt_files = paths;
        sources.files.extend(parent.files);
    } else {
        let path = "prompts/saved-system.md".to_string();
        sources.files.insert(path.clone(), saved_parent.into());
        sources.verbatim.insert(path.clone());
        imported.system_prompt_files = vec![path];
        imported.system_prompt.clear();
    }
    ensure!(
        assemble(
            &imported.system_prompt,
            &imported.system_prompt_files,
            sources
        )? == saved_parent,
        "migration changed saved prompt"
    );
    validate(&imported)?;
    Ok((imported, !copied))
}

fn validate_nested_keys(value: &serde_yaml::Value) -> anyhow::Result<()> {
    use serde_yaml::Value;
    fn keys(value: &Value, allowed: &[&str]) -> anyhow::Result<()> {
        if let Some(map) = value.as_mapping() {
            for key in map.keys() {
                ensure!(
                    key.as_str().is_some_and(|s| allowed.contains(&s)),
                    "unknown field {key:?}"
                );
            }
        }
        Ok(())
    }
    fn presets(value: &Value) -> anyhow::Result<()> {
        if let Some(map) = value.as_mapping() {
            for preset in map.values() {
                keys(
                    preset,
                    &[
                        "model",
                        "max_spawn_depth",
                        "tools",
                        "role_suffix",
                        "tool_profile",
                    ],
                )?;
            }
        }
        Ok(())
    }
    fn grants(value: &Value) -> anyhow::Result<()> {
        keys(value, &["shell", "network", "fs"])?;
        keys(&value["shell"], &["allow", "deny"])?;
        keys(&value["network"], &["endpoints", "allow_private"])?;
        keys(&value["fs"], &["allow_read", "allow_write"])?;
        if let Some(endpoints) = value["network"]["endpoints"].as_sequence() {
            for endpoint in endpoints {
                keys(endpoint, &["host", "path_prefix", "methods"])?;
            }
        }
        Ok(())
    }
    presets(&value["presets"])?;
    grants(&value["capabilities"])?;
    if let Some(map) = value["grants"].as_mapping() {
        for grant in map.values() {
            grants(grant)?;
        }
    }
    if let Some(workers) = value["workers"].as_sequence() {
        for worker in workers {
            keys(
                worker,
                &[
                    "name",
                    "system_prompt",
                    "system_prompt_files",
                    "model",
                    "tools",
                    "max_spawn_depth",
                    "presets",
                ],
            )?;
            presets(&worker["presets"])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scope_replacement_is_strict_and_preserves_unrelated_state() {
        let mut old = AgentDbConfig {
            autonomous: true,
            model: Some("old".into()),
            default_memory_banks: vec!["keep".into()],
            applied_config_hash: Some("history".into()),
            ..Default::default()
        };
        old.capabilities.fs = Some(crate::grants::FsGrant {
            allow_write: crate::grants::Allowlist::Only(vec![]),
            ..Default::default()
        });
        let yaml = "version: 1\nagent_id: stable\nautonomous: false\ntools: []\nmodel: null\nmax_spawn_depth: 10\n";
        let doc = AgentDocument::parse(yaml, "stable").unwrap();
        let cfg = doc
            .replace_scope(
                &old,
                PromptSources {
                    version: VERSION,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!cfg.autonomous);
        assert_eq!(cfg.tools, Some(vec![]));
        assert_eq!(cfg.model, None);
        assert_eq!(cfg.max_spawn_depth, Some(10));
        assert_eq!(cfg.capabilities, Grants::default());
        assert_eq!(cfg.default_memory_banks, old.default_memory_banks);
        assert_eq!(cfg.applied_config_hash, old.applied_config_hash);
        for bad in [
            "unknown: true",
            "autonomous: true\nautonomous: false",
            "capabilities:\n  network:\n    allow_privat: true",
            "presets:\n  p:\n    unknown: true",
            "grants:\n  shell:\n    fs: {}\n  shell:\n    fs: {}",
            "workers:\n- name: a\n  unknown: true",
        ] {
            assert!(
                AgentDocument::parse(&format!("version: 1\nagent_id: stable\n{bad}\n"), "stable")
                    .is_err(),
                "accepted {bad}"
            );
        }
        assert!(AgentDocument::parse(yaml, "other").is_err());
        assert!(AgentDocument::parse(&yaml.replace("version: 1", "version: 2"), "stable").is_err());
    }

    #[test]
    fn portable_parent_and_worker_sources_preserve_order_and_exact_migration() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("one.md");
        let second = dir.path().join("two.md");
        std::fs::write(&first, "FIRST\n").unwrap();
        std::fs::write(&second, "SECOND\n").unwrap();
        let cfg = AgentDbConfig {
            system_prompt: "INLINE  \n".into(),
            system_prompt_files: vec![first.display().to_string(), second.display().to_string()],
            workers: vec![WorkerDbConfig {
                name: "helper".into(),
                system_prompt_files: vec![second.display().to_string()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let saved = "FIRST\n\nSECOND\n\nINLINE  \n";
        let (copied, flattened) = migrate(&cfg, saved).unwrap();
        assert!(!flattened);
        let serialized = serde_json::to_string(&copied).unwrap();
        assert!(!serialized.contains(dir.path().to_str().unwrap()));
        let (flat, flattened) = migrate(&cfg, "OLD SAVED  \n").unwrap();
        assert!(flattened);
        assert_eq!(
            assemble(
                &flat.system_prompt,
                &flat.system_prompt_files,
                flat.prompt_sources.as_ref().unwrap()
            )
            .unwrap(),
            "OLD SAVED  \n"
        );
        std::fs::remove_file(&first).unwrap();
        std::fs::remove_file(&second).unwrap();
        let remote: AgentDbConfig = serde_json::from_str(&serialized).unwrap();
        let sources = remote.prompt_sources.as_ref().unwrap();
        assert_eq!(
            assemble(&remote.system_prompt, &remote.system_prompt_files, sources).unwrap(),
            saved
        );
        assert_eq!(
            assemble(
                &remote.workers[0].system_prompt,
                &remote.workers[0].system_prompt_files,
                sources
            )
            .unwrap(),
            "SECOND"
        );
        assert_eq!(std::fs::read_to_string(&first).ok(), None);
        assert!(
            migrate(&cfg, saved).is_err(),
            "missing worker source must block"
        );
    }

    #[test]
    fn managed_import_rejects_escaping_special_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "../secret",
            "/tmp/secret",
            "./x",
            "a/../x",
            "a//x",
            "a/./x",
            ".projection.json",
            "agent.yaml",
            "agent.yaml/x",
            "x\\y",
        ] {
            assert!(validate_source_path(path).is_err(), "accepted {path}");
        }
        let outside = tempfile::NamedTempFile::new().unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), dir.path().join("alias")).unwrap();
            assert!(source_path(dir.path(), "alias").is_err());
            std::os::unix::fs::symlink(outside.path().parent().unwrap(), dir.path().join("dir"))
                .unwrap();
            assert!(source_path(dir.path(), "dir/secret").is_err());
        }
        assert!(source_path(dir.path(), "missing").is_err());
        std::fs::create_dir(dir.path().join("directory")).unwrap();
        assert!(source_path(dir.path(), "directory").is_err());
    }
}
