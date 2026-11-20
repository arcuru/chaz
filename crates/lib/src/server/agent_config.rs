//! Operator-only configuration transactions and portable folder lifecycle.

use super::Server;
use crate::agent_db::{AgentDb, AgentDbConfig};
use crate::agent_folder::AgentFolders;
use crate::hosted_index::DbEntry;
use anyhow::{Context, ensure};
use eidetica::entry::ID;
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

    /// Resolve operator folder references without using the name-only routing
    /// winner. Full IDs take precedence over display names.
    pub(crate) fn resolve_agent_folder_ref(&self, reference: &str) -> anyhow::Result<DbEntry> {
        if let Ok(id) = ID::parse(reference)
            && !id.is_empty()
        {
            return self.config_entry(&id);
        }
        self.resolve_agent_folder_name(reference)
    }

    /// Resolve a display name (for example, the name on a Peer Settings row).
    pub fn resolve_agent_folder_name(&self, name: &str) -> anyhow::Result<DbEntry> {
        let mut matches = self.agent_index.entries_named(name).into_iter();
        let entry = matches
            .next()
            .with_context(|| format!("No hosted agent matches '{name}' (see /agent hosted)"))?;
        ensure!(
            matches.next().is_none(),
            "Agent name '{name}' is ambiguous; use an Agent DB ID from /agent hosted"
        );
        Ok(entry)
    }

    /// Cached presentation only: drawing the TUI must not read the filesystem.
    pub fn agent_folder_status(&self, db_id: &ID) -> String {
        let Some(entry) = self.agent_index.find_by_id(db_id) else {
            return "agent DB is not hosted on this peer".into();
        };
        if let Some((name, status)) = self
            .agent_folder_statuses
            .read()
            .expect("folder status lock")
            .get(db_id)
            && *name == entry.display_name
        {
            return status.clone();
        }
        match self.agent_folders() {
            Some(folders) => format!(
                "{} (not initialized; /agent project {db_id})",
                folders
                    .folder(&entry.display_name, &db_id.to_string())
                    .display()
            ),
            None => "agent folder interface not configured on this peer".into(),
        }
    }

    fn record_folder_status(&self, entry: &DbEntry, error: Option<&str>) {
        if let Some(folders) = self.agent_folders() {
            let id = entry.db_id.to_string();
            let name = &entry.display_name;
            let status = match error {
                Some(error) => format!(
                    "{} (projection/migration blocked: {error}; retry /agent project {id})",
                    folders.folder(name, &id).display()
                ),
                None => folders.status(name, &id),
            };
            self.agent_folder_statuses
                .write()
                .expect("folder status lock")
                .insert(entry.db_id.clone(), (entry.display_name.clone(), status));
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
        // Runtime routing still uses its existing name mapping. Keep the
        // migration diagnostic on the same DB that was opened.
        let entry = self
            .agent_index
            .find_by_name(&agent.name)
            .context("no hosted agent with that name")?;
        let db = self.config_db(&entry).await?;
        let cfg = db.read_config().await?;
        ensure!(
            cfg.prompt_sources.is_some(),
            "agent '{}' configuration migration blocked: {}; recover saved prompt sources before execution",
            agent.name,
            self.agent_folder_status(&entry.db_id)
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

    fn config_entry(&self, id: &ID) -> anyhow::Result<DbEntry> {
        self.agent_index
            .find_by_id(id)
            .with_context(|| format!("Agent DB {id} is not hosted on this peer"))
    }

    async fn config_db(&self, entry: &DbEntry) -> anyhow::Result<AgentDb> {
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
        entry: &DbEntry,
        db: &AgentDb,
        old: &AgentDbConfig,
        cfg: &AgentDbConfig,
        folders: Option<&AgentFolders>,
        flattened: bool,
        mode: ProjectionMode,
    ) -> anyhow::Result<AgentConfigOutcome> {
        let name = &entry.display_name;
        let id = entry.db_id.to_string();
        crate::agent_bundle::validate(cfg)?;
        if mode == ProjectionMode::Import
            && let Some(folders) = folders
        {
            folders.prepare_import_locked(name, &id, cfg).context(
                "reload recovery intent could not be recorded; configuration not committed",
            )?;
        }
        let changed = cfg != old;
        if changed {
            db.write_config(cfg).await?;
        }
        let mut runtime = self.agents.build_from_db_config(name, cfg);
        runtime.system_prompt = self.resolve_db_prompt(cfg).await;
        // Do not replace the name-keyed runtime snapshot when projecting or
        // editing a different DB with the same display name.
        if self
            .agent_index
            .find_by_name(name)
            .is_some_and(|routed| routed.db_id == entry.db_id)
        {
            self.agents.upsert(runtime);
        }
        let projection_error = folders
            .and_then(|folders| {
                let project = || -> anyhow::Result<()> {
                    folders.project_locked(name, &id, cfg, mode == ProjectionMode::Force)?;
                    Ok(())
                };
                project().err()
            })
            .map(|e| format!("{e:#}"));
        self.record_folder_status(entry, projection_error.as_deref());
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
        db_id: &ID,
        import_parent_files: bool,
        prompt_edit: bool,
        edit: F,
    ) -> anyhow::Result<AgentConfigOutcome>
    where
        F: FnOnce(&mut AgentDbConfig) -> anyhow::Result<()>,
    {
        let entry = self.config_entry(db_id)?;
        let db = self.config_db(&entry).await?;
        let id = db_id.to_string();
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
                    folders
                        .as_ref()
                        .map(|f| f.folder(&entry.display_name, &id))
                        .as_deref(),
                )?;
            }
        } else if prompt_edit {
            self.refresh_prompt_ref(&mut cfg).await?;
        }
        self.commit_and_project(
            &entry,
            &db,
            &old,
            &cfg,
            folders.as_deref(),
            flattened,
            ProjectionMode::Changed,
        )
        .await
    }

    pub async fn reload_agent_folder(&self, db_id: &ID) -> anyhow::Result<AgentConfigOutcome> {
        let folders = self
            .agent_folders()
            .context("agent folder interface not configured on this peer")?;
        let entry = self.config_entry(db_id)?;
        let db = self.config_db(&entry).await?;
        let id = db_id.to_string();
        let lock = self.config_lock(&id).await;
        let _guard = lock.lock().await;
        let _file_lock = folders.lock_wait(&id).await?;
        let old = db.read_config().await?;
        ensure!(
            old.prompt_sources.is_some(),
            "agent has not migrated; use /agent project {id} first"
        );
        let cfg = folders.read_locked(&entry.display_name, &id, &old)?;
        self.commit_and_project(
            &entry,
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
        db_id: &ID,
        force: bool,
    ) -> anyhow::Result<AgentConfigOutcome> {
        let folders = self
            .agent_folders()
            .context("agent folder interface not configured on this peer")?;
        let entry = self.config_entry(db_id)?;
        let db = self.config_db(&entry).await?;
        let id = db_id.to_string();
        let lock = self.config_lock(&id).await;
        let _guard = lock.lock().await;
        let _file_lock = folders.lock_wait(&id).await?;
        let old = db.read_config().await?;
        let (cfg, flattened) = self.portable_config(&old).await?;
        self.commit_and_project(
            &entry,
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
    pub async fn watch_agent_config(&self, db_id: &ID) -> anyhow::Result<()> {
        if self.agent_folders().is_none() {
            return Ok(());
        }
        let db = self.config_db(&self.config_entry(db_id)?).await?;
        let id = db_id.to_string();
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
        let db_id = db_id.clone();
        self.track_task(tokio::spawn(async move {
            while rx.recv().await.is_some() {
                let Some(server) = weak.upgrade() else { break; };
                match server.project_agent_config(&db_id, false).await {
                    Ok(outcome) if outcome.projection_error.is_none() => {},
                    Ok(outcome) => tracing::warn!(%db_id, error = ?outcome.projection_error, "agent DB authoritative; folder projection pending"),
                    Err(error) => {
                        if let Some(entry) = server.agent_index.find_by_id(&db_id) {
                            server.record_folder_status(&entry, Some(&format!("{error:#}")));
                        }
                        tracing::warn!(%db_id, %error, "agent folder synchronization failed");
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
            if let Err(error) = self.watch_agent_config(&entry.db_id).await {
                tracing::warn!(agent = %name, %error, "cannot watch agent configuration");
                continue;
            }
            match self.project_agent_config(&entry.db_id, false).await {
                Ok(outcome) if outcome.projection_error.is_none() => {
                    if outcome.flattened {
                        tracing::warn!(agent = %name, "preserved saved prompt as one source; original decomposition unavailable");
                    }
                }
                Ok(outcome) => {
                    tracing::warn!(agent = %name, error = ?outcome.projection_error, "agent DB authoritative; folder projection pending")
                }
                Err(error) => {
                    self.record_folder_status(&entry, Some(&format!("{error:#}")));
                    tracing::warn!(agent = %name, %error, "agent folder migration/projection blocked; original config preserved");
                }
            }
        }
    }
}
