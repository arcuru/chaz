//! Operator-only configuration transactions and portable folder lifecycle.

use super::Server;
use crate::agent_db::{AgentDb, AgentDbConfig};
use crate::agent_folder::AgentFolders;
use anyhow::{Context, ensure};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Default)]
pub struct AgentConfigOutcome {
    pub changed: bool,
    pub flattened: bool,
    /// A DB commit can succeed while its outward projection fails. Returning
    /// that separately prevents callers from claiming the active state rolled
    /// back or suggesting a stale-file reload as recovery.
    pub projection_error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectionMode {
    Changed,
    Force,
    Import,
}

impl Server {
    pub fn set_agent_config_root(&self, root: PathBuf) {
        *self.agent_config_root.write().expect("agent root lock") = Some(root.clone());
        *self.agent_folders.write().expect("agent folders lock") =
            Some(Arc::new(AgentFolders::new(root)));
    }

    pub fn agent_folders(&self) -> Option<Arc<AgentFolders>> {
        self.agent_folders
            .read()
            .expect("agent folders lock")
            .clone()
    }

    /// Cached presentation only: drawing the TUI must not read the filesystem.
    pub fn agent_folder_status(&self, name: &str) -> String {
        if let Some(status) = self
            .agent_folder_statuses
            .read()
            .expect("folder status lock")
            .get(name)
        {
            return status.clone();
        }
        match (self.agent_folders(), self.agent_index.find_by_name(name)) {
            (Some(folders), Some(entry)) => format!(
                "{} (not initialized; /agent project {name})",
                folders.folder(name, &entry.db_id.to_string()).display()
            ),
            _ => "agent folder interface not configured on this peer".into(),
        }
    }

    fn record_folder_status(&self, name: &str, error: Option<&str>) {
        if let (Some(folders), Some(entry)) =
            (self.agent_folders(), self.agent_index.find_by_name(name))
        {
            let id = entry.db_id.to_string();
            let status = match error {
                Some(error) => format!(
                    "{} (projection/migration blocked: {error}; retry /agent project {name})",
                    folders.folder(name, &id).display()
                ),
                None => folders.status(name, &id),
            };
            self.agent_folder_statuses
                .write()
                .expect("folder status lock")
                .insert(name.into(), status);
        }
    }

    /// A configured folder peer must not run a guessed legacy prompt when
    /// migration reported missing saved content. Legacy library callers with
    /// no folder root retain their existing hydration behavior.
    pub(super) async fn agent_for_execution(
        &self,
        agent: crate::agent::Agent,
    ) -> anyhow::Result<crate::agent::Agent> {
        if self.agent_folders().is_none() {
            return Ok(self.hydrate_agent_from_db(agent).await);
        }
        let db = self.config_db(&agent.name).await?;
        let cfg = db.read_config().await?;
        ensure!(
            cfg.prompt_sources.is_some(),
            "agent '{}' configuration migration blocked: {}; recover saved prompt sources before execution",
            agent.name,
            self.agent_folder_status(&agent.name)
        );
        let rebuilt = self.agents.build_from_db_config(&agent.name, &cfg);
        self.agents.upsert(rebuilt.clone());
        Ok(rebuilt)
    }

    async fn config_lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.agent_config_locks
            .lock()
            .await
            .entry(id.into())
            .or_default()
            .clone()
    }

    async fn config_db(&self, name: &str) -> anyhow::Result<AgentDb> {
        let entry = self
            .agent_index
            .find_by_name(name)
            .context("no hosted agent with that name")?;
        self.registry
            .open_agent_db(&entry.db_id, Some(&entry.pubkey))
            .await?
            .context("this peer holds no writable agent key")
    }

    async fn portable_config(
        &self,
        current: &AgentDbConfig,
    ) -> anyhow::Result<(AgentDbConfig, bool)> {
        if current.prompt_sources.is_some() {
            crate::agent_bundle::validate(current)?;
            return Ok((current.clone(), false));
        }
        let saved = if let Some(snapshot) = &current.system_prompt_ref {
            self.fetch_resolved_prompt(snapshot).await.context("saved prompt snapshot unavailable on this peer; preserve legacy config and recover the original snapshot before migration")?
        } else if current.system_prompt_files.is_empty() {
            current.system_prompt.clone()
        } else {
            // With no saved snapshot the legacy runtime uses these files.
            // Every source must be available to establish its effective text.
            let mut copy = current.clone();
            crate::agent_bundle::import_sources(&mut copy)?;
            crate::agent_bundle::assemble(
                &copy.system_prompt,
                &copy.system_prompt_files,
                copy.prompt_sources.as_ref().unwrap(),
            )?
        };
        crate::agent_bundle::migrate(current, &saved)
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_and_project(
        &self,
        name: &str,
        db: &AgentDb,
        old: &AgentDbConfig,
        cfg: &AgentDbConfig,
        folders: Option<&AgentFolders>,
        flattened: bool,
        mode: ProjectionMode,
    ) -> anyhow::Result<AgentConfigOutcome> {
        crate::agent_bundle::validate(cfg)?;
        if mode == ProjectionMode::Import
            && let Some(folders) = folders
        {
            folders
                .prepare_import_locked(name, &db.id().to_string(), cfg)
                .context(
                    "reload recovery intent could not be recorded; configuration not committed",
                )?;
        }
        let changed = cfg != old;
        if changed {
            db.write_config(cfg).await?;
        }
        let mut runtime = self.agents.build_from_db_config(name, cfg);
        runtime.system_prompt = self.resolve_db_prompt(cfg).await;
        self.agents.upsert(runtime);
        let projection_error = folders
            .and_then(|folders| {
                let project = || -> anyhow::Result<()> {
                    folders.project_locked(
                        name,
                        &db.id().to_string(),
                        cfg,
                        mode == ProjectionMode::Force,
                    )?;
                    Ok(())
                };
                project().err()
            })
            .map(|e| format!("{e:#}"));
        self.record_folder_status(name, projection_error.as_deref());
        Ok(AgentConfigOutcome {
            changed,
            flattened,
            projection_error,
        })
    }

    /// All scalar/list operator edits share validation, commit and projection.
    /// Only an explicit source-list import reads files; ordinary setting and
    /// inline edits reuse synchronized text, including pending local drafts.
    pub async fn update_agent_config<F>(
        &self,
        name: &str,
        import_parent_files: bool,
        prompt_edit: bool,
        edit: F,
    ) -> anyhow::Result<AgentConfigOutcome>
    where
        F: FnOnce(&mut AgentDbConfig) -> anyhow::Result<()>,
    {
        let db = self.config_db(name).await?;
        let id = db.id().to_string();
        let lock = self.config_lock(&id).await;
        let _guard = lock.lock().await;
        let folders = self.agent_folders();
        let _file_lock = if let Some(folders) = &folders {
            Some(folders.lock_wait(&id).await?)
        } else {
            None
        };
        let old = db.read_config().await?;
        let (mut cfg, flattened) = if folders.is_some() {
            self.portable_config(&old).await?
        } else {
            (old.clone(), false)
        };
        edit(&mut cfg)?;
        if cfg.prompt_sources.is_some() {
            if import_parent_files {
                crate::agent_bundle::import_parent_sources(
                    &mut cfg,
                    folders.as_ref().map(|f| f.folder(name, &id)).as_deref(),
                )?;
            }
        } else if prompt_edit {
            self.refresh_prompt_ref(&mut cfg).await?;
        }
        self.commit_and_project(
            name,
            &db,
            &old,
            &cfg,
            folders.as_deref(),
            flattened,
            ProjectionMode::Changed,
        )
        .await
    }

    pub async fn reload_agent_folder(&self, name: &str) -> anyhow::Result<AgentConfigOutcome> {
        let folders = self
            .agent_folders()
            .context("agent folder interface not configured on this peer")?;
        let db = self.config_db(name).await?;
        let id = db.id().to_string();
        let lock = self.config_lock(&id).await;
        let _guard = lock.lock().await;
        let _file_lock = folders.lock_wait(&id).await?;
        let old = db.read_config().await?;
        ensure!(
            old.prompt_sources.is_some(),
            "agent has not migrated; use /agent project {name} first"
        );
        let cfg = folders.read_locked(name, &id, &old)?;
        self.commit_and_project(
            name,
            &db,
            &old,
            &cfg,
            Some(&folders),
            false,
            ProjectionMode::Import,
        )
        .await
    }

    /// Initial migration, ordinary config notifications and explicit outward
    /// repair use the same path. Nothing imports local folder contents here.
    pub async fn project_agent_config(
        &self,
        name: &str,
        force: bool,
    ) -> anyhow::Result<AgentConfigOutcome> {
        let folders = self
            .agent_folders()
            .context("agent folder interface not configured on this peer")?;
        let db = self.config_db(name).await?;
        let id = db.id().to_string();
        let lock = self.config_lock(&id).await;
        let _guard = lock.lock().await;
        let _file_lock = folders.lock_wait(&id).await?;
        let old = db.read_config().await?;
        let (cfg, flattened) = self.portable_config(&old).await?;
        self.commit_and_project(
            name,
            &db,
            &old,
            &cfg,
            Some(&folders),
            flattened,
            if force {
                ProjectionMode::Force
            } else {
                ProjectionMode::Changed
            },
        )
        .await
    }

    /// Source-agnostic local/synced DB notifications, coalesced per agent. The
    /// deterministic exported identity ignores unrelated Agent DB writes.
    pub async fn watch_agent_config(&self, name: &str) -> anyhow::Result<()> {
        if self.agent_folders().is_none() {
            return Ok(());
        }
        let db = self.config_db(name).await?;
        let id = db.id().to_string();
        let mut watched = self.watched_agent_configs.lock().await;
        if watched.contains(&id) {
            return Ok(());
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        let subscription = db
            .database()
            .on_write(move |_, _| {
                let _ = tx.try_send(());
                Box::pin(async { Ok(()) })
            })
            .await?;
        subscription.detach();
        watched.insert(id);
        drop(watched);
        let weak = self.self_ref.clone();
        let name = name.to_string();
        self.track_task(tokio::spawn(async move {
            while rx.recv().await.is_some() {
                let Some(server) = weak.upgrade() else { break; };
                match server.project_agent_config(&name, false).await {
                    Ok(outcome) if outcome.projection_error.is_none() => {},
                    Ok(outcome) => tracing::warn!(agent = %name, error = ?outcome.projection_error, "agent DB authoritative; folder projection pending"),
                    Err(error) => {
                        server.record_folder_status(&name, Some(&format!("{error:#}")));
                        tracing::warn!(agent = %name, %error, "agent folder synchronization failed");
                    },
                }
            }
        }));
        Ok(())
    }

    pub async fn initialize_agent_folders(&self) {
        if self.agent_folders().is_none() {
            return;
        }
        for entry in self.agent_index.list() {
            let name = &entry.display_name;
            // Subscribe first, then read the latest coherent config: a sync
            // racing startup cannot fall into a read-before-subscribe gap.
            if let Err(error) = self.watch_agent_config(name).await {
                tracing::warn!(agent = %name, %error, "cannot watch agent configuration");
                continue;
            }
            match self.project_agent_config(name, false).await {
                Ok(outcome) if outcome.projection_error.is_none() => {
                    if outcome.flattened {
                        tracing::warn!(agent = %name, "preserved saved prompt as one source; original decomposition unavailable");
                    }
                }
                Ok(outcome) => {
                    tracing::warn!(agent = %name, error = ?outcome.projection_error, "agent DB authoritative; folder projection pending")
                }
                Err(error) => {
                    self.record_folder_status(name, Some(&format!("{error:#}")));
                    tracing::warn!(agent = %name, %error, "agent folder migration/projection blocked; original config preserved");
                }
            }
        }
    }
}
