//! Recoverable DB-to-folder projection. The DB commit and these individual
//! atomic file replacements are deliberately not described as one transaction.

use crate::agent_bundle::{AgentDocument, validate_source_path};
use crate::agent_db::AgentDbConfig;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

const STATE: &str = ".projection.json";

#[derive(Debug, Clone)]
pub struct AgentFolders {
    root: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Projection {
    agent_id: String,
    completed: Option<String>,
    managed: BTreeSet<String>,
    pending: Option<String>,
    pending_files: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    import: Option<ImportIntent>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportIntent {
    revision: String,
    files: BTreeSet<String>,
}

impl AgentFolders {
    /// Explicitly injected: constructing a library Server never writes under
    /// the real user's home. Only executable setup chooses the default root.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn folder(&self, name: &str, id: &str) -> PathBuf {
        let slug: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .take(60)
            .collect();
        let hash = blake3::hash(id.as_bytes()).to_hex();
        self.root.join(format!(
            "{}-{}",
            if slug.is_empty() { "agent" } else { &slug },
            &hash[..16]
        ))
    }

    /// File locks serialize separate trusted CLI/daemon processes using the
    /// same local root. A crash releases the OS lock; no stale PID bookkeeping.
    fn lock_file(&self, id: &str) -> anyhow::Result<File> {
        checked_dir(&self.root, true)?;
        let locks = self.root.join(".locks");
        checked_dir(&locks, true)?;
        let path = locks.join(blake3::hash(id.as_bytes()).to_hex());
        if path.exists() {
            regular_file(&path)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(file)
    }

    pub fn lock(&self, id: &str) -> anyhow::Result<File> {
        let file = self.lock_file(id)?;
        file.try_lock()
            .context("agent configuration operation already in progress")?;
        Ok(file)
    }

    /// Park, don't drop a DB notification or block the async executor, when
    /// another process temporarily owns this same peer-local folder.
    pub async fn lock_wait(&self, id: &str) -> anyhow::Result<File> {
        let file = self.lock_file(id)?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }

    pub fn status(&self, name: &str, id: &str) -> String {
        let folder = self.folder(name, id);
        match self.read_state(&folder, id) {
            Ok(state) if state.pending.is_some() => format!(
                "{} (projection pending; retry /agent project {name})",
                folder.display()
            ),
            Ok(state) if state.completed.is_some() => format!(
                "{} (projected; local edits import only on reload)",
                folder.display()
            ),
            Ok(_) => format!("{} (not projected)", folder.display()),
            Err(e) => format!("{} (unavailable: {e:#})", folder.display()),
        }
    }

    fn read_state(&self, folder: &Path, id: &str) -> anyhow::Result<Projection> {
        checked_dir(folder, false)?;
        let path = folder.join(STATE);
        regular_file(&path)?;
        let state: Projection = serde_json::from_slice(&std::fs::read(&path)?)?;
        ensure!(
            state.agent_id == id,
            "managed directory belongs to another agent"
        );
        for path in state.managed.iter().chain(&state.pending_files) {
            if path != "agent.yaml" {
                validate_source_path(path)?;
            }
        }
        if let Some(import) = &state.import {
            for path in &import.files {
                validate_source_path(path)?;
            }
        }
        Ok(state)
    }

    /// Caller holds `lock(id)` across read/commit/project. A notification uses
    /// the same lock, rereads the latest DB config, and projects only its actual
    /// config/source revision. Nothing here writes to the Agent DB.
    pub fn project_locked(
        &self,
        name: &str,
        id: &str,
        cfg: &AgentDbConfig,
        force: bool,
    ) -> anyhow::Result<bool> {
        crate::agent_bundle::validate(cfg)?;
        let sources = cfg
            .prompt_sources
            .as_ref()
            .context("agent has no migrated source bundle")?;
        let document = AgentDocument::from_config(id, cfg);
        let revision = revision(&document, sources)?;
        let folder = self.folder(name, id);
        let mut state = if folder.try_exists()? {
            self.read_state(&folder, id)?
        } else {
            // Prepare the identity journal before advertising the folder.
            // A crash before rename leaves only an unadvertised staging dir;
            // after rename, every partial projection has a recoverable marker.
            let staging = self
                .root
                .join(format!(".provision-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&staging)?;
            let state = Projection {
                agent_id: id.into(),
                pending: Some(revision.clone()),
                ..Default::default()
            };
            atomic_write(&staging.join(STATE), &serde_json::to_vec(&state)?)?;
            ensure!(
                !folder.try_exists()?,
                "agent folder appeared during provisioning"
            );
            std::fs::rename(&staging, &folder)?;
            File::open(&self.root)?.sync_all()?;
            state
        };
        if !force && state.completed.as_ref() == Some(&revision) && state.pending.is_none() {
            if state.import.take().is_some() {
                atomic_write(&folder.join(STATE), &serde_json::to_vec(&state)?)?;
            }
            return Ok(false);
        }

        let mut files: BTreeMap<String, String> = sources.files.clone();
        files.insert("agent.yaml".into(), serde_yaml::to_string(&document)?);
        let mut previously_managed: BTreeSet<_> =
            state.managed.union(&state.pending_files).cloned().collect();
        // A reload records custody before committing. Only the exact accepted
        // revision can promote it; a rejected import must not adopt its drafts.
        if let Some(import) = &state.import
            && import.revision == revision
        {
            previously_managed.extend(import.files.iter().cloned());
        }
        // Persist outward-repair intent even when target preflight fails. Do
        // not claim new files as managed until collision checks succeed.
        state.pending = Some(revision.clone());
        atomic_write(&folder.join(STATE), &serde_json::to_vec(&state)?)?;
        // Refuse collisions before writing any part of the projection. Unknown
        // files in the folder survive; they are never recursively ingested/deleted.
        for path in files.keys() {
            let target = folder.join(path);
            check_parent_chain(&target)?;
            if std::fs::symlink_metadata(&target).is_ok() {
                regular_file(&target)?;
                ensure!(
                    previously_managed.contains(path),
                    "refusing to overwrite unrelated file {}",
                    target.display()
                );
            }
        }
        for path in &previously_managed {
            let target = folder.join(path);
            check_parent_chain(&target)?;
            if std::fs::symlink_metadata(&target).is_ok() {
                regular_file(&target)?;
            }
        }
        state.pending_files = files.keys().cloned().collect();
        state.import = None;
        atomic_write(&folder.join(STATE), &serde_json::to_vec(&state)?)?;
        for (relative, contents) in &files {
            let path = folder.join(relative);
            checked_dir(path.parent().expect("managed file parent"), true)?;
            atomic_write(&path, contents.as_bytes())?;
        }
        for obsolete in previously_managed.difference(&state.pending_files) {
            let path = folder.join(obsolete);
            if path.try_exists()? {
                std::fs::remove_file(path)?;
            }
        }
        state.completed = Some(revision);
        state.managed = std::mem::take(&mut state.pending_files);
        state.pending = None;
        atomic_write(&folder.join(STATE), &serde_json::to_vec(&state)?)?;
        Ok(true)
    }

    /// Persist reload intent before the DB commit. Recovery adopts these
    /// paths only if the authoritative DB accepts this exact revision.
    pub fn prepare_import_locked(
        &self,
        name: &str,
        id: &str,
        cfg: &AgentDbConfig,
    ) -> anyhow::Result<()> {
        let folder = self.folder(name, id);
        let mut state = self.read_state(&folder, id)?;
        let sources = cfg
            .prompt_sources
            .as_ref()
            .context("missing source bundle")?;
        state.import = Some(ImportIntent {
            revision: revision(&AgentDocument::from_config(id, cfg), sources)?,
            files: sources.files.keys().cloned().collect(),
        });
        atomic_write(&folder.join(STATE), &serde_json::to_vec(&state)?)
    }

    /// Reload never consumes an incomplete exporter. Repair is always outward
    /// from the authoritative DB, not an import of stale/half-projected files.
    pub fn read_locked(
        &self,
        name: &str,
        id: &str,
        old: &AgentDbConfig,
    ) -> anyhow::Result<AgentDbConfig> {
        let folder = self.folder(name, id);
        let state = self.read_state(&folder, id)?;
        ensure!(
            state.pending.is_none() && state.completed.is_some(),
            "projection incomplete at {}; run /agent project {name} before reload",
            folder.display()
        );
        let sources = old
            .prompt_sources
            .as_ref()
            .context("agent has no portable source bundle")?;
        let accepted_revision = revision(&AgentDocument::from_config(id, old), sources)?;
        ensure!(
            state.completed.as_ref() == Some(&accepted_revision),
            "folder is behind accepted DB configuration at {}; run /agent project {name} before reload",
            folder.display()
        );
        let path = folder.join("agent.yaml");
        regular_file(&path)?;
        let document = AgentDocument::parse(&std::fs::read_to_string(&path)?, id)
            .with_context(|| format!("parse {}", path.display()))?;
        let sources = document.read_sources(&folder, old.prompt_sources.as_ref())?;
        document.replace_scope(old, sources)
    }
}

fn revision(
    document: &AgentDocument,
    sources: &crate::agent_bundle::PromptSources,
) -> anyhow::Result<String> {
    let mut value = serde_json::to_value((document, sources))?;
    value.sort_all_objects();
    Ok(blake3::hash(&serde_json::to_vec(&value)?)
        .to_hex()
        .to_string())
}

fn regular_file(path: &Path) -> anyhow::Result<()> {
    ensure!(
        std::fs::symlink_metadata(path)?.is_file(),
        "not a regular managed file: {}",
        path.display()
    );
    Ok(())
}

fn check_parent_chain(path: &Path) -> anyhow::Result<()> {
    checked_dir(path.parent().context("missing file parent")?, false).or_else(|e| {
        if !path.parent().unwrap().exists() {
            // Missing directories are allowed during preflight, but existing
            // ancestors must still be ordinary directories, never symlinks.
            let mut parent = path.parent().unwrap();
            while !parent.exists() {
                parent = parent.parent().context("missing ancestor")?;
            }
            checked_dir(parent, false)
        } else {
            Err(e)
        }
    })
}

/// Walk the actual path, including ancestors outside the agent folder, rather
/// than checking just a leaf after following a symlink to somewhere unrelated.
pub(crate) fn checked_dir(path: &Path, create: bool) -> anyhow::Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        ensure!(
            !matches!(component, Component::ParentDir),
            "unnormalized managed directory"
        );
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "not an ordinary managed directory: {}",
                current.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                match std::fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        checked_dir(&current, false)?
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e).with_context(|| format!("inspect {}", current.display())),
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("missing write parent")?;
    let temp = parent.join(format!(".projection-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("project {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_bundle::{PromptSources, VERSION};

    fn config() -> AgentDbConfig {
        AgentDbConfig {
            system_prompt: "INLINE".into(),
            system_prompt_files: vec!["prompts/one.md".into()],
            prompt_sources: Some(PromptSources {
                version: VERSION,
                files: BTreeMap::from([("prompts/one.md".into(), "FIRST\n".into())]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn projection_restart_reload_and_content_identity() {
        let root = tempfile::tempdir().unwrap();
        let folders = AgentFolders::new(root.path().join("agents"));
        let cfg = config();
        let folder = folders.folder("helper", "stable");
        let _lock = folders.lock("stable").unwrap();
        assert!(
            folders
                .project_locked("helper", "stable", &cfg, false)
                .unwrap()
        );
        let mut newer = cfg.clone();
        newer.model = Some("new DB revision not yet exported".into());
        assert!(
            folders.read_locked("helper", "stable", &newer).is_err(),
            "must not import stale folder over a newer accepted DB revision"
        );
        std::fs::write(folder.join("prompts/one.md"), "PENDING\n").unwrap();
        std::fs::write(folder.join("agent.yaml"), "invalid: [").unwrap();
        let restarted = AgentFolders::new(folders.root.clone());
        assert!(
            !restarted
                .project_locked("helper", "stable", &cfg, false)
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("prompts/one.md")).unwrap(),
            "PENDING\n"
        );
        assert!(restarted.read_locked("helper", "stable", &cfg).is_err());
        assert!(
            restarted
                .project_locked("helper", "stable", &cfg, true)
                .unwrap()
        );
        std::fs::write(folder.join("prompts/one.md"), "EDITED\n").unwrap();
        let imported = restarted.read_locked("helper", "stable", &cfg).unwrap();
        assert_eq!(
            imported.prompt_sources.as_ref().unwrap().files["prompts/one.md"],
            "EDITED\n"
        );
        let mut unrelated = cfg.clone();
        unrelated.default_memory_banks.push("not-exported".into());
        unrelated.applied_config_hash = Some("old".into());
        assert!(
            !folders
                .project_locked("helper", "stable", &unrelated, false)
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("prompts/one.md")).unwrap(),
            "EDITED\n"
        );
        unrelated.model = Some("accepted-update".into());
        assert!(
            folders
                .project_locked("helper", "stable", &unrelated, false)
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("prompts/one.md")).unwrap(),
            "FIRST\n"
        );
    }

    #[test]
    fn interrupted_projection_is_recoverable_and_only_removes_managed_files() {
        let root = tempfile::tempdir().unwrap();
        let folders = AgentFolders::new(root.path().join("agents"));
        let mut cfg = config();
        let _lock = folders.lock("stable").unwrap();
        folders
            .project_locked("helper", "stable", &cfg, false)
            .unwrap();
        let folder = folders.folder("helper", "stable");
        std::fs::write(folder.join("notes.txt"), "KEEP").unwrap();
        // A persisted pending journal is the actual crash-recovery boundary.
        let mut state = folders.read_state(&folder, "stable").unwrap();
        state.pending = Some("interrupted".into());
        state.pending_files.insert("prompts/half-written.md".into());
        atomic_write(&folder.join(STATE), &serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::write(folder.join("prompts/half-written.md"), "PARTIAL").unwrap();
        std::fs::write(folder.join("agent.yaml"), "partial").unwrap();
        assert!(folders.status("helper", "stable").contains("pending"));
        assert!(folders.read_locked("helper", "stable", &cfg).is_err());
        folders
            .project_locked("helper", "stable", &cfg, false)
            .unwrap();
        assert!(!folder.join("prompts/half-written.md").exists());
        assert_eq!(
            std::fs::read_to_string(folder.join("notes.txt")).unwrap(),
            "KEEP"
        );
        cfg.system_prompt_files.clear();
        cfg.prompt_sources.as_mut().unwrap().files.clear();
        folders
            .project_locked("helper", "stable", &cfg, false)
            .unwrap();
        assert!(!folder.join("prompts/one.md").exists());
        assert!(folder.join("notes.txt").exists());
    }

    #[test]
    fn interrupted_reload_recovers_only_accepted_import_custody() {
        for committed in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let folders = AgentFolders::new(root.path().join("agents"));
            let cfg = config();
            let lock = folders.lock("stable").unwrap();
            folders
                .project_locked("helper", "stable", &cfg, false)
                .unwrap();
            let folder = folders.folder("helper", "stable");
            std::fs::write(folder.join("prompts/added.md"), "NEW SOURCE").unwrap();
            let mut document = AgentDocument::from_config("stable", &cfg);
            document.system_prompt_files.push("prompts/added.md".into());
            let draft = serde_yaml::to_string(&document).unwrap();
            std::fs::write(folder.join("agent.yaml"), &draft).unwrap();
            let imported = folders.read_locked("helper", "stable", &cfg).unwrap();
            folders
                .prepare_import_locked("helper", "stable", &imported)
                .unwrap();
            // Stop before export. The DB either accepted the imported revision
            // or still contains the original; the durable journal must handle both.
            drop(lock);
            let restarted = AgentFolders::new(folders.root.clone());
            let _lock = restarted.lock("stable").unwrap();
            if committed {
                assert!(
                    restarted
                        .project_locked("helper", "stable", &imported, false)
                        .unwrap()
                );
                assert_eq!(
                    restarted
                        .read_locked("helper", "stable", &imported)
                        .unwrap(),
                    imported
                );
            } else {
                assert!(
                    !restarted
                        .project_locked("helper", "stable", &cfg, false)
                        .unwrap()
                );
                assert_eq!(
                    std::fs::read_to_string(folder.join("agent.yaml")).unwrap(),
                    draft
                );
                let mut changed = cfg.clone();
                changed.model = Some("another accepted revision".into());
                restarted
                    .project_locked("helper", "stable", &changed, false)
                    .unwrap();
                assert_eq!(
                    std::fs::read_to_string(folder.join("prompts/added.md")).unwrap(),
                    "NEW SOURCE"
                );
                assert!(
                    restarted
                        .project_locked("helper", "stable", &imported, false)
                        .is_err(),
                    "an uncommitted import must not adopt an otherwise unrelated file"
                );
            }
        }
    }

    #[test]
    fn projection_refuses_foreign_directories_aliases_and_untracked_collisions() {
        let root = tempfile::tempdir().unwrap();
        let folders = AgentFolders::new(root.path().join("agents"));
        let _lock = folders.lock("stable").unwrap();
        let folder = folders.folder("helper", "stable");
        std::fs::create_dir(&folder).unwrap();
        assert!(
            folders
                .project_locked("helper", "stable", &config(), false)
                .is_err()
        );
        std::fs::remove_dir(&folder).unwrap();
        folders
            .project_locked("helper", "stable", &config(), false)
            .unwrap();
        assert!(folders.read_state(&folder, "another-identity").is_err());
        let mut cfg = config();
        cfg.system_prompt_files.push("prompts/unrelated.md".into());
        cfg.prompt_sources
            .as_mut()
            .unwrap()
            .files
            .insert("prompts/unrelated.md".into(), "NEW".into());
        std::fs::write(folder.join("prompts/unrelated.md"), "KEEP").unwrap();
        assert!(
            folders
                .project_locked("helper", "stable", &cfg, false)
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("prompts/unrelated.md")).unwrap(),
            "KEEP"
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(folder.join("prompts/one.md")).unwrap();
            std::os::unix::fs::symlink(folder.join("notes.txt"), folder.join("prompts/one.md"))
                .unwrap();
            assert!(
                folders
                    .project_locked("helper", "stable", &config(), true)
                    .is_err()
            );
        }
    }
}
