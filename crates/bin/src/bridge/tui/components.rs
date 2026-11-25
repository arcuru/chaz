//! Small read-only projections independent of transcript hydration.
use super::*;
use chaz_core::agent_db::AgentDb;
use chaz_core::hosted_index::DbEntry;
use eidetica::auth::crypto::PublicKey;
use eidetica::entry::ID;
use eidetica::{Database, Snapshot};
use presentation::Latest;
use tokio::sync::Notify;

struct Watch {
    id: u64,
    latest: Arc<Latest<Result<Value, String>>>,
    _task: supervised::Task<()>,
}

pub(super) struct Update {
    id: u64,
    result: Result<Value, String>,
}

#[cfg(test)]
impl Update {
    pub(super) fn is_ancestry(&self) -> bool {
        matches!(self.result, Ok(Value::Ancestry(_)))
    }

    pub(super) fn is_catalog(&self) -> bool {
        matches!(self.result, Ok(Value::Group(..)))
    }
}

#[derive(Clone)]
enum Source {
    Agent(Box<DbEntry>),
    Group(GroupView, Arc<presentation::Preparations>),
    Row { id: String, worker: bool },
}

#[derive(Clone, PartialEq, Eq)]
enum GroupView {
    Picker(Option<String>),
    SessionSettings,
}

enum Value {
    Agent(Defaults),
    /// No groups means the rows do not depend on ancestry, or must not wait for it.
    Group(Vec<SessionIndex>, Option<workers::Groups>),
    Ancestry(workers::Groups),
    Row(RowFields),
}

#[derive(Default)]
pub(super) struct Defaults {
    pub(super) model: Option<String>,
    pub(super) cap: Option<usize>,
}

pub(super) struct RowFields {
    name: Option<String>,
    agent: Option<String>,
    description: Option<String>,
}

impl From<&SessionMeta> for RowFields {
    fn from(meta: &SessionMeta) -> Self {
        Self {
            name: meta.name.clone(),
            agent: meta.agent_name.clone(),
            description: meta.description.clone(),
        }
    }
}

impl RowFields {
    pub(super) fn project(&self, row: &mut SessionInfo, alias: Option<&str>) {
        row.name = self.name.clone().or_else(|| alias.map(str::to_owned));
        row.agent_name.clone_from(&self.agent);
        row.description.clone_from(&self.description);
        row.loaded = true;
    }
}

impl Source {
    async fn open(&self, server: &Server) -> anyhow::Result<Database> {
        match self {
            Source::Agent(entry) => Ok(server
                .registry()
                .open_agent_db(&entry.db_id, Some(&entry.pubkey))
                .await?
                .ok_or_else(|| anyhow::anyhow!("Agent DB key unavailable"))?
                .database()
                .clone()),
            Source::Group(..) => Ok(server.registry().chaz_group().clone()),
            Source::Row { id, worker: true } => {
                Ok(server.registry().open_worker_session(id).await?.1)
            }
            Source::Row { id, worker: false } => {
                server.registry().bind_session_for_observation(id).await
            }
        }
    }

    async fn read(
        &self,
        server: &Arc<Server>,
        db: Database,
        tips: Snapshot,
    ) -> anyhow::Result<Value> {
        // Embedded store reads do not enforce per-handle access the way service
        // reads do. A retained frontier or previously valid key grants nothing.
        if db.instance()?.remote_connection().is_none() {
            db.current_permission().await?;
        }
        match self {
            Source::Agent(_) => {
                let config = AgentDb::from_database(db).read_config_at(&tips).await?;
                Ok(Value::Agent(Defaults {
                    model: config.model,
                    cap: config.max_context_tokens,
                }))
            }
            Source::Group(view, pool) => {
                let indices = server.registry().list_sessions_at(&tips).await?;
                let (rows, groups) = match view {
                    GroupView::Picker(Some(parent)) => {
                        let (rows, groups) =
                            workers::linked_workers(server, indices, parent, pool.clone())
                                .await
                                .map_err(anyhow::Error::msg)?;
                        (rows, Some(groups))
                    }
                    GroupView::Picker(None) | GroupView::SessionSettings => (indices, None),
                };
                Ok(Value::Group(rows, groups))
            }
            Source::Row { worker, .. } => {
                if *worker {
                    chaz_core::session::jobs::require_observer_write(&db).await?;
                }
                Ok(Value::Row(RowFields::from(
                    &chaz_core::session::try_read_meta_at(&db, &tips).await?,
                )))
            }
        }
    }
}

impl Watch {
    fn start(
        source: Source,
        server: Arc<Server>,
        pool: Arc<presentation::Preparations>,
        wake: Arc<Notify>,
    ) -> Self {
        let id = presentation::next_id();
        let latest = Arc::new(Latest::new(wake));
        let output = latest.clone();
        let task = tokio::spawn(async move {
            let registered = async {
                // Ordinary historical binding can hold the account mutex and
                // must be preemptible by New Session. Once subscribed, reads
                // and registration (which may verify) are owned and drained.
                let db = if matches!(source, Source::Row { worker: false, .. }) {
                    let _permit = pool.acquire().await?;
                    tokio::time::timeout(supervised::READ_TIMEOUT, source.open(&server))
                        .await
                        .map_err(|_| "Session metadata open timed out".to_string())?
                        .map_err(stringify)?
                } else {
                    let source = source.clone();
                    let server = server.clone();
                    pool.prepare(async move { source.open(&server).await.map_err(stringify) })
                        .await?
                };
                let read_pool = pool.clone();
                let latest = output.clone();
                let read_db = db.clone();
                pool.prepare(async move {
                    let (callback, _) = presentation::subscribe(&db, latest, move |tips| {
                        let (source, db, server) =
                            (source.clone(), read_db.clone(), server.clone());
                        let pool = read_pool.clone();
                        async move {
                            pool.prepare(async move {
                                source.read(&server, db, tips).await.map_err(stringify)
                            })
                            .await
                        }
                    })
                    .await
                    .map_err(stringify)?;
                    Ok(callback)
                })
                .await
            }
            .await;
            match registered {
                Ok(_callback) => std::future::pending::<()>().await,
                Err(error) => output.publish(Err(error)),
            }
        });
        Self {
            id,
            latest,
            _task: supervised::Task(task),
        }
    }

    /// One check of the Workers in an accepted catalog. A newer catalog replaces
    /// this watch, so its result can never be applied to rows it did not check.
    fn ancestry(
        workers: Vec<String>,
        server: Arc<Server>,
        pool: Arc<presentation::Preparations>,
        wake: Arc<Notify>,
    ) -> Self {
        let latest = Arc::new(Latest::new(wake));
        let output = latest.clone();
        let task = tokio::spawn(async move {
            let groups = workers::ancestry(server, workers, pool).await;
            output.publish(Ok(Value::Ancestry(groups)));
        });
        Self {
            id: presentation::next_id(),
            latest,
            _task: supervised::Task(task),
        }
    }

    fn take(&self) -> Option<Update> {
        self.latest.take().map(|result| Update {
            id: self.id,
            result,
        })
    }
}

fn stringify(error: anyhow::Error) -> String {
    format!("{error:#}")
}

struct GroupWatch {
    generation: u64,
    view: GroupView,
    watch: Watch,
    /// Retired with the view or connection, and replaced by each catalog.
    ancestry: Option<Watch>,
}

impl GroupWatch {
    fn is_current(&self, app: &App) -> bool {
        self.generation == app.session_generation && group_view(app).as_ref() == Some(&self.view)
    }
}

pub(super) struct Watches {
    pool: Arc<presentation::Preparations>,
    row_pool: Arc<presentation::Preparations>,
    /// Separate so a stuck check cannot hold up catalog or agent reads.
    ancestry_pool: Arc<presentation::Preparations>,
    group: Option<GroupWatch>,
    rows: HashMap<String, (bool, Watch)>,
    agents: HashMap<ID, (DbEntry, Watch)>,
    wake: Arc<Notify>,
}

impl Watches {
    pub(super) fn new() -> Self {
        Self {
            pool: Arc::new(presentation::Preparations::new(4)),
            row_pool: Arc::new(presentation::Preparations::new(4)),
            ancestry_pool: Arc::new(presentation::Preparations::new(4)),
            group: None,
            rows: HashMap::new(),
            agents: HashMap::new(),
            wake: Arc::new(Notify::new()),
        }
    }

    pub(super) fn disconnect(&mut self) {
        self.agents.clear();
        self.group = None;
        self.rows.clear();
    }

    pub(super) async fn shutdown(&mut self) {
        self.disconnect();
        tokio::join!(
            self.pool.drain(),
            self.row_pool.drain(),
            self.ancestry_pool.drain()
        );
    }

    pub(super) fn sync_picker(&mut self, app: &mut App, server: &Arc<Server>) {
        self.retire_picker_if_hidden(app);
        let Some(view) = group_view(app) else { return };
        // Anything still current survived the retirement above.
        if self.group.is_none() {
            self.rows.clear();
            let watch = Watch::start(
                Source::Group(view.clone(), self.ancestry_pool.clone()),
                server.clone(),
                self.pool.clone(),
                self.wake.clone(),
            );
            self.group = Some(GroupWatch {
                generation: app.session_generation,
                view,
                watch,
                ancestry: None,
            });
            app.session_catalog_loading = app.mode == TuiMode::SessionPicker;
        }
        if app.mode != TuiMode::SessionPicker {
            return;
        }
        let desired = visible_rows(app);
        self.rows
            .retain(|id, (worker, _)| desired.get(id) == Some(worker));
        app.session_row_errors
            .retain(|id, _| desired.contains_key(id));
        for (id, worker) in desired {
            self.rows.entry(id.clone()).or_insert_with(|| {
                let watch = Watch::start(
                    Source::Row { id, worker },
                    server.clone(),
                    self.row_pool.clone(),
                    self.wake.clone(),
                );
                (worker, watch)
            });
        }
    }

    pub(super) fn retire_picker_if_hidden(&mut self, app: &App) {
        if self
            .group
            .as_ref()
            .is_some_and(|group| !group.is_current(app))
        {
            self.group = None;
            self.rows.clear();
        }
    }

    pub(super) fn sync_agents(&mut self, app: &mut App, server: &Arc<Server>) {
        if !app.backend_available || !server.is_startup_ready() {
            return;
        }
        let index = server.agent_index();
        let relevant_names = app
            .session_list
            .iter()
            .skip(app.picker_visible_rows.start)
            .take(app.picker_visible_rows.len())
            .filter(|_| app.mode == TuiMode::SessionPicker)
            .filter_map(|row| row.agent_name.as_deref());
        let mut desired = HashMap::new();
        let mut add = |entry: DbEntry| {
            desired.insert(entry.db_id.clone(), entry);
        };
        for tab in &app.tabs {
            for agent in &tab.live.meta.agents {
                if let Ok(id) = ID::parse(&agent.db_id)
                    && let Some(entry) = index.find_by_id(&id)
                {
                    add(entry);
                }
            }
            if let Some(entry) = select_agent(server, &tab.live.meta, &tab.live.writer_keys).1 {
                add(entry);
            }
        }
        let visible_settings = app
            .visible_peer_agents
            .clone()
            .filter_map(|i| app.peer_agents_names.get(i))
            .map(String::as_str);
        let picker_agent = match (&app.model_picker_scope, app.mode) {
            (ModelPickerScope::AgentGlobal(name), TuiMode::ModelPicker) => Some(name.as_str()),
            _ => None,
        };
        for name in server
            .default_agents()
            .iter()
            .map(String::as_str)
            .chain(relevant_names)
            .chain(visible_settings)
            .chain(picker_agent)
        {
            if let Some(entry) = index.find_by_name(name) {
                add(entry);
            }
        }
        self.agents
            .retain(|id, (entry, _)| desired.get(id) == Some(entry));
        app.agent_defaults.retain(|id, _| desired.contains_key(id));
        app.agent_config_errors
            .retain(|id, _| desired.contains_key(id));
        for (id, entry) in desired {
            self.agents.entry(id).or_insert_with(|| {
                let watch = Watch::start(
                    Source::Agent(Box::new(entry.clone())),
                    server.clone(),
                    self.pool.clone(),
                    self.wake.clone(),
                );
                (entry, watch)
            });
        }
    }

    fn active_watches(&self) -> impl Iterator<Item = &Watch> {
        self.group
            .iter()
            .flat_map(|group| [Some(&group.watch), group.ancestry.as_ref()])
            .flatten()
            .chain(self.rows.values().map(|(_, watch)| watch))
            .chain(self.agents.values().map(|(_, watch)| watch))
    }

    pub(super) async fn changed(&self) {
        self.wake.notified().await;
    }

    pub(super) fn apply_latest(
        &mut self,
        app: &mut App,
        server: &Arc<Server>,
        backend: &BackendManager,
    ) -> bool {
        let updates: Vec<_> = self.active_watches().filter_map(Watch::take).collect();
        let changed = !updates.is_empty();
        for update in updates {
            self.apply(app, server, backend, update);
        }
        changed
    }

    #[cfg(test)]
    pub(super) fn try_next(&self) -> Option<Update> {
        self.active_watches().find_map(Watch::take)
    }

    #[cfg(test)]
    pub(super) async fn next(&self) -> Update {
        loop {
            if let Some(update) = self.try_next() {
                return update;
            }
            self.changed().await;
        }
    }

    pub(super) fn apply(
        &mut self,
        app: &mut App,
        server: &Arc<Server>,
        backend: &BackendManager,
        update: Update,
    ) {
        if !app.backend_available {
            return;
        }
        if let Some(group) = self
            .group
            .as_mut()
            .filter(|group| group.watch.id == update.id && group.is_current(app))
        {
            match (&group.view, update.result) {
                (GroupView::Picker(_), Ok(Value::Group(indices, Some(groups)))) => {
                    apply_session_catalog(app, indices);
                    workers::group(app, groups);
                }
                (GroupView::Picker(_), Ok(Value::Group(indices, None))) => {
                    let ids = workers::worker_ids(&indices);
                    // Grouping this view already accepted stays until its
                    // replacement settles; another view's is never inherited.
                    let mut groups = match group.ancestry.take() {
                        Some(_) => std::mem::take(&mut app.worker_groups),
                        None => workers::Groups::default(),
                    };
                    workers::mark_pending(&mut groups, &ids);
                    apply_session_catalog(app, indices);
                    workers::group(app, groups);
                    group.ancestry = (!ids.is_empty()).then(|| {
                        Watch::ancestry(
                            ids,
                            server.clone(),
                            self.ancestry_pool.clone(),
                            self.wake.clone(),
                        )
                    });
                }
                (GroupView::SessionSettings, Ok(Value::Group(indices, _))) => {
                    if let Some(snapshot) = &mut app.session_settings_snapshot {
                        snapshot.created_at = indices
                            .iter()
                            .find(|i| i.session_db_id == snapshot.session_db_id)
                            .and_then(|i| i.created_at);
                    }
                }
                (GroupView::Picker(_), Err(error)) => {
                    app.session_catalog_loading = false;
                    app.session_picker_error = Some(error);
                }
                (GroupView::SessionSettings, Err(error)) => {
                    tracing::warn!(%error, "Session index observation failed")
                }
                _ => unreachable!(),
            }
        } else if self.group.as_ref().is_some_and(|group| {
            group.ancestry.as_ref().is_some_and(|w| w.id == update.id) && group.is_current(app)
        }) {
            match update.result {
                Ok(Value::Ancestry(groups)) => workers::group(app, groups),
                _ => unreachable!(),
            }
        } else if let Some((id, (worker, _))) = self
            .rows
            .iter()
            .find(|(_, (_, watch))| watch.id == update.id)
            && app.mode == TuiMode::SessionPicker
            && app.tab_index_for(id).is_none()
            && app.session_indices.get(id).and_then(row_kind) == Some(*worker)
        {
            match update.result {
                Ok(Value::Row(meta)) => {
                    app.session_row_errors.remove(id);
                    project_row(app, id, meta);
                }
                Err(error) => {
                    app.session_row_errors.insert(id.clone(), error);
                }
                _ => unreachable!(),
            }
        } else if let Some((id, _)) = self
            .agents
            .iter()
            .find(|(_, (_, watch))| watch.id == update.id)
        {
            match update.result {
                Ok(Value::Agent(value)) => {
                    app.agent_defaults.insert(id.clone(), value);
                    app.agent_config_errors.remove(id);
                    refresh::derive_models(app, server, backend);
                    app.update_picker_pin(server);
                }
                Err(error) => {
                    tracing::warn!(agent_db = %id, %error, "Agent configuration observation failed");
                    app.agent_config_errors.insert(id.clone(), error);
                    refresh::derive_models(app, server, backend);
                }
                _ => unreachable!(),
            }
        }
    }
}

fn group_view(app: &App) -> Option<GroupView> {
    if !app.backend_available || app.operations.is_creating_session() {
        return None;
    }
    match app.mode {
        TuiMode::SessionPicker => Some(GroupView::Picker(app.workers_filter.clone())),
        TuiMode::Settings(SettingsScope::Session) => Some(GroupView::SessionSettings),
        _ => None,
    }
}

/// Outer None means there is no accepted component yet; inner None is the
/// actual stored default (use the backend's default model).
pub(super) fn default_model(app: &App, server: &Server, name: &str) -> Option<Option<String>> {
    if let Some(entry) = server.agent_index().find_by_name(name) {
        return app
            .agent_defaults
            .get(&entry.db_id)
            .map(|defaults| defaults.model.clone());
    }
    #[cfg(test)]
    {
        server.agents().get(name).map(|agent| agent.default_model)
    }
    #[cfg(not(test))]
    None
}

pub(super) fn visible_rows(app: &App) -> HashMap<String, bool> {
    app.picker_visible_rows
        .clone()
        .filter_map(|index| app.session_list.get(index))
        .filter(|row| app.tab_index_for(&row.session_db_id).is_none())
        .filter_map(|row| app.session_indices.get(&row.session_db_id))
        .filter_map(|index| Some((index.session_db_id.clone(), row_kind(index)?)))
        .collect()
}

fn row_kind(index: &SessionIndex) -> Option<bool> {
    let source = index.source.as_deref();
    (source != Some("spawn:reference"))
        .then(|| chaz_core::session::workers::is_worker_source(source))
}

/// Match Chaz's existing session resolution, using membership and legacy
/// names from the delivered Session snapshot. The registry supplies local
/// identity/availability only; hosted model configuration comes from the DB.
pub(super) fn select_agent(
    server: &Server,
    meta: &SessionMeta,
    keys: &[PublicKey],
) -> (String, Option<DbEntry>) {
    let index = server.agent_index();
    let agents = server.agents();
    let hosted = keys
        .iter()
        .filter_map(|key| index.find_by_pubkey(key))
        .find(|entry| agents.get(&entry.display_name).is_some());
    if let Some(entry) = hosted {
        return (entry.display_name.clone(), Some(entry));
    }
    let name = meta
        .agent_name
        .as_deref()
        .and_then(|name| agents.get(name))
        .or_else(|| agents.default_agent())
        .map(|agent| agent.name)
        .unwrap_or_default();
    let entry = index.find_by_name(&name);
    (name, entry)
}
