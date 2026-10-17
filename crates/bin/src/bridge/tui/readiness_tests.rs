//! Real storage opens behind a deterministic gate; no executor or model.
use super::*;
use chaz_core::server::{BuildOptions, BuiltServer, McpReadiness};
use eidetica::{
    Instance, NewAccount,
    backend::{
        BackendImpl, InstanceMetadata, InstanceSecrets, RecordMutations, RecordPage, RecordRange,
        RecordView, StagingToken, StoreStateRequest, VerificationStatus, database::InMemory,
    },
    entry::{Entry, ID},
    snapshot::Snapshot,
};
use std::sync::Mutex;
use tokio::sync::{Notify, Semaphore, oneshot};

struct Probe {
    roots: Mutex<HashSet<ID>>,
    accessed: Mutex<HashSet<ID>>,
    blocked: Mutex<Option<ID>>,
    entered: Notify,
    release: Semaphore,
}
impl Default for Probe {
    fn default() -> Self {
        Self {
            roots: Mutex::new(HashSet::new()),
            accessed: Mutex::new(HashSet::new()),
            blocked: Mutex::new(None),
            entered: Notify::new(),
            release: Semaphore::new(0),
        }
    }
}
impl Probe {
    fn record(&self, id: &ID) {
        if self.roots.lock().unwrap().contains(id) {
            self.accessed.lock().unwrap().insert(id.clone());
        }
    }
}
struct Storage {
    inner: InMemory,
    probe: Arc<Probe>,
}
// Delegate the native backend unchanged except for root-access observation and
// one blocked historical open. Default trait fallbacks would change the fixture.
#[async_trait::async_trait]
impl BackendImpl for Storage {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn get(&self, id: &ID) -> eidetica::Result<Entry> {
        self.probe.record(id);
        let blocked = self.probe.blocked.lock().unwrap().as_ref() == Some(id);
        if blocked {
            self.probe.entered.notify_one();
            let _permit = self.probe.release.acquire().await.unwrap();
        }
        self.inner.get(id).await
    }
    async fn get_verification_status(&self, id: &ID) -> eidetica::Result<VerificationStatus> {
        self.inner.get_verification_status(id).await
    }
    async fn put(&self, entry: Entry) -> eidetica::Result<()> {
        self.inner.put(entry).await
    }
    async fn update_verification_status(
        &self,
        id: &ID,
        status: VerificationStatus,
    ) -> eidetica::Result<()> {
        self.inner.update_verification_status(id, status).await
    }
    async fn get_entries_by_verification_status(
        &self,
        status: VerificationStatus,
    ) -> eidetica::Result<Vec<ID>> {
        self.inner.get_entries_by_verification_status(status).await
    }
    async fn snapshot(&self, tree: &ID) -> eidetica::Result<Snapshot> {
        self.probe.record(tree);
        self.inner.snapshot(tree).await
    }
    async fn store_snapshot(&self, tree: &ID, store: &str) -> eidetica::Result<Snapshot> {
        self.probe.record(tree);
        self.inner.store_snapshot(tree, store).await
    }
    async fn store_snapshot_at(
        &self,
        tree: &ID,
        store: &str,
        snapshot: &Snapshot,
    ) -> eidetica::Result<Snapshot> {
        self.probe.record(tree);
        self.inner.store_snapshot_at(tree, store, snapshot).await
    }
    async fn all_roots(&self) -> eidetica::Result<Vec<ID>> {
        self.inner.all_roots().await
    }
    async fn find_merge_base(
        &self,
        tree: &ID,
        store: &str,
        entry_ids: &[ID],
    ) -> eidetica::Result<Option<ID>> {
        self.probe.record(tree);
        self.inner.find_merge_base(tree, store, entry_ids).await
    }
    async fn get_tree(&self, tree: &ID) -> eidetica::Result<Vec<Entry>> {
        self.probe.record(tree);
        self.inner.get_tree(tree).await
    }
    async fn get_store(&self, tree: &ID, store: &str) -> eidetica::Result<Vec<Entry>> {
        self.probe.record(tree);
        self.inner.get_store(tree, store).await
    }
    async fn get_tree_from_tips(&self, tree: &ID, tips: &[ID]) -> eidetica::Result<Vec<Entry>> {
        self.probe.record(tree);
        self.inner.get_tree_from_tips(tree, tips).await
    }
    async fn store_at(
        &self,
        tree: &ID,
        store: &str,
        snapshot: &Snapshot,
    ) -> eidetica::Result<Vec<Entry>> {
        self.probe.record(tree);
        self.inner.store_at(tree, store, snapshot).await
    }
    async fn get_sorted_store_parents(
        &self,
        tree: &ID,
        id: &ID,
        store: &str,
    ) -> eidetica::Result<Vec<ID>> {
        self.probe.record(tree);
        self.inner.get_sorted_store_parents(tree, id, store).await
    }
    async fn get_path_from_to(
        &self,
        tree: &ID,
        store: &str,
        from: Option<&ID>,
        to: &[ID],
    ) -> eidetica::Result<Vec<ID>> {
        self.probe.record(tree);
        self.inner.get_path_from_to(tree, store, from, to).await
    }
    async fn get_instance_metadata(&self) -> eidetica::Result<Option<InstanceMetadata>> {
        self.inner.get_instance_metadata().await
    }
    async fn set_instance_metadata(&self, metadata: &InstanceMetadata) -> eidetica::Result<()> {
        self.inner.set_instance_metadata(metadata).await
    }
    async fn get_instance_secrets(&self) -> eidetica::Result<Option<InstanceSecrets>> {
        self.inner.get_instance_secrets().await
    }
    async fn set_instance_secrets(&self, secrets: &InstanceSecrets) -> eidetica::Result<()> {
        self.inner.set_instance_secrets(secrets).await
    }
    async fn resolve_store_state(
        &self,
        request: &StoreStateRequest,
    ) -> eidetica::Result<Option<RecordView>> {
        self.probe.record(&request.database);
        self.inner.resolve_store_state(request).await
    }
    async fn begin_store_state_staging(
        &self,
        request: StoreStateRequest,
    ) -> eidetica::Result<StagingToken> {
        self.inner.begin_store_state_staging(request).await
    }
    async fn stage_store_state_records(
        &self,
        token: &StagingToken,
        mutations: RecordMutations,
    ) -> eidetica::Result<()> {
        self.inner.stage_store_state_records(token, mutations).await
    }
    async fn publish_store_state(&self, token: StagingToken) -> eidetica::Result<RecordView> {
        self.inner.publish_store_state(token).await
    }
    async fn abort_store_state(&self, token: StagingToken) -> eidetica::Result<()> {
        self.inner.abort_store_state(token).await
    }
    async fn store_state_record_get(
        &self,
        view: &RecordView,
        key: &[u8],
    ) -> eidetica::Result<Option<Vec<u8>>> {
        self.inner.store_state_record_get(view, key).await
    }
    async fn store_state_record_scan(
        &self,
        view: &RecordView,
        range: &RecordRange,
        after: Option<&[u8]>,
        limit: usize,
    ) -> eidetica::Result<RecordPage> {
        self.inner
            .store_state_record_scan(view, range, after, limit)
            .await
    }
}

async fn runtime(dir: &std::path::Path, probe: Arc<Probe>) -> BuiltServer {
    let (instance, mut user) = Instance::create_backend(
        Box::new(Storage {
            inner: InMemory::new(),
            probe,
        }),
        NewAccount::passwordless("session-readiness"),
    )
    .await
    .unwrap();
    for name in ["default", "selected"] {
        chaz_core::agent_db::create_agent_db(
            &mut user,
            name,
            &Default::default(),
            &chaz_core::agent_db::AgentMeta {
                display_name: Some(name.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let mut config: Config = serde_yaml::from_str(&format!(
        "execution: client\nstate_dir: {0}\neidetica:\n  connection: sqlite://{0}/unused.db\n  login:\n    username: session-readiness\n    passwordless: true\nagents:\n  - name: default\n  - name: selected\ndefault_agents: [default]\nagent_groups:\n  chosen: [selected]\n",
        dir.display()
    )).unwrap();
    chaz_core::server::build(
        &mut config,
        instance,
        user,
        BuildOptions {
            config_path: dir.join("config.yaml"),
            run_routine_engine: false,
            bootstrap_agents_from_config: false,
            run_agent_loop: false,
            extra_auto_approved_tools: Vec::new(),
            mcp_readiness: McpReadiness::Deferred,
            targeted_session: None,
        },
    )
    .await
    .unwrap()
}

async fn settle(app: &mut App) {
    while app.operations.is_pending() {
        let completion =
            tokio::time::timeout(std::time::Duration::from_secs(2), app.operations.recv())
                .await
                .expect("creation waited for historical loading")
                .unwrap();
        operations::apply(app, completion);
    }
}
async fn receive(rx: &mut mpsc::Receiver<SessionLoad>) -> SessionLoad {
    tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
fn frame(app: &mut App, server: &Arc<Server>, backend: &BackendManager) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|f| view::ui(f, app, server, backend, &Config::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..24)
        .map(|y| {
            (0..100)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn new_session(app: &mut App, server: &Arc<Server>, backend: &BackendManager) {
    let (approval, _) = mpsc::channel(8);
    let (notify, _) = mpsc::channel(64);
    let selected =
        input::handle_picker_key(app, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .unwrap();
    dispatch_picker_selection(selected, app, server, backend, &approval, &notify);
}

#[tokio::test]
async fn new_session_preempts_a_blocked_historical_open_without_history_growth() {
    for count in [1, 32, 128] {
        let dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe::default());
        let built = runtime(dir.path(), probe.clone()).await;
        let server = &built.server;
        let backend = BackendManager::new(&None, built.secret_store.clone());
        for n in 0..count {
            let (_, db) = server
                .registry()
                .create_session(Some("history"))
                .await
                .unwrap();
            probe.roots.lock().unwrap().insert(db.root_id().clone());
            // Unrelated tracked non-session databases must not be discovered either.
            let mut user = server.registry().user_lock().await;
            let key = user.get_default_key().unwrap();
            let mut settings = eidetica::crdt::Doc::new();
            settings.set("name", format!("unrelated-{n}").as_str());
            let db = user.create_database(settings, &key).await.unwrap();
            probe.roots.lock().unwrap().insert(db.root_id().clone());
        }
        let indices = server.registry().list_sessions().await.unwrap();
        let blocked = indices[0].session_db_id.clone();
        let mut app = App::hub(HashSet::new());
        apply_session_catalog(&mut app, indices.clone());
        // The same production metadata loader enters the real open_session I/O,
        // while holding the account-session mutex that create_session will need.
        *probe.blocked.lock().unwrap() = Some(ID::parse(&blocked).unwrap());
        let (tx, mut rx) = mpsc::channel(128);
        let registry = server.registry_arc();
        spawn_session_metadata_load(
            &mut app,
            vec![indices[0].clone()],
            tx.clone(),
            move |index| {
                let registry = registry.clone();
                async move {
                    commands::try_load_session_metadata(&registry, index)
                        .await
                        .map_err(|e| e.to_string())
                }
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), probe.entered.notified())
            .await
            .unwrap();
        assert!(
            probe
                .accessed
                .lock()
                .unwrap()
                .contains(&ID::parse(&blocked).unwrap()),
            "probe must detect a real historical open"
        );
        probe.accessed.lock().unwrap().clear();
        assert!(frame(&mut app, server, &backend).contains("New session"));
        let generation = app.session_generation;
        new_session(&mut app, server, &backend);
        new_session(&mut app, server, &backend);
        assert!(
            app.operations
                .notice
                .as_deref()
                .unwrap()
                .contains("already pending")
        );
        // Repeated frames must not restart historical reads during creation.
        ensure_view(&mut app, server, &tx);
        request_session_metadata(&mut app, server, &tx);
        settle(&mut app).await;
        assert_eq!(app.mode, TuiMode::Chat);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active().current_agent, "default");
        assert_eq!(app.active().live.meta.agents[0].display_name, "default");
        assert!(
            app.active().live.observation.is_some(),
            "composer requires observation setup"
        );
        for c in "ready-draft-界".chars() {
            input::handle_chat_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        assert!(frame(&mut app, server, &backend).contains("ready-draft-界"));
        assert_eq!(app.input, "ready-draft-界");
        assert!(
            probe.accessed.lock().unwrap().is_empty(),
            "creation accessed unrelated databases"
        );
        assert_eq!(
            probe.release.available_permits(),
            0,
            "test released the historical gate"
        );
        assert!(rx.try_recv().is_err(), "blocked metadata completed");
        // Pre-create successes and failures cannot change the new view.
        assert!(matches!(
            apply_session_catalog_result(&mut app, generation, Err("obsolete failure".into())),
            CatalogLoadOutcome::Ignored
        ));
        assert!(app.session_picker_error.is_none());
        assert!(matches!(
            apply_session_catalog_result(&mut app, generation, Ok(indices)),
            CatalogLoadOutcome::Ignored
        ));
        let created = app.active().session_db_id.clone();
        app.cancel_session_fill();
        *probe.blocked.lock().unwrap() = None;
        open_session_picker(&mut app, server, &tx);
        let SessionLoad::Catalog { generation, result } =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("expected reconciled catalog")
        };
        assert!(matches!(
            apply_session_catalog_result(&mut app, generation, result),
            CatalogLoadOutcome::Loaded
        ));
        assert_eq!(app.session_list.len(), count + 1);
        assert_eq!(
            app.session_list
                .iter()
                .filter(|r| r.session_db_id == created)
                .count(),
            1
        );
        println!(
            "history sessions={count}, unrelated DBs={count}: creation unrelated accesses=0; correct default, composer and reconciliation"
        );
        drop(app);
        built.shutdown().await;
    }
}

#[tokio::test]
async fn new_session_finishes_before_catalog_completion_and_failure() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path(), Arc::new(Probe::default())).await;
    let server = &built.server;
    server.set_default_agents(vec!["selected".into()]);
    let backend = BackendManager::new(&None, built.secret_store.clone());
    let mut app = App::hub(HashSet::new());
    let (tx, mut rx) = mpsc::channel(128);
    let (mut release, gate) = oneshot::channel::<()>();
    let (entered, started) = oneshot::channel();
    app.session_catalog_loading = true;
    spawn_session_catalog_load(&mut app, tx.clone(), async move {
        entered.send(()).unwrap();
        gate.await.map_err(|e| e.to_string())?;
        Ok(Vec::new())
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    assert!(frame(&mut app, server, &backend).contains("Loading sessions"));
    new_session(&mut app, server, &backend);
    settle(&mut app).await;
    assert_eq!(app.active().current_agent, "selected");
    assert_eq!(app.active().live.meta.agents[0].display_name, "selected");
    assert!(app.active().live.observation.is_some());
    assert_eq!(server.registry().list_sessions().await.unwrap().len(), 1);
    tokio::time::timeout(std::time::Duration::from_secs(2), release.closed())
        .await
        .expect("creation did not cancel its obsolete catalog");
    assert!(rx.try_recv().is_err());
    open_session_picker(&mut app, server, &tx);
    let generation = app.session_generation;
    app.cancel_session_fill();
    app.mode = TuiMode::SessionPicker;
    spawn_session_catalog_load(&mut app, tx.clone(), async {
        Err("historical catalog unavailable".into())
    });
    let SessionLoad::Catalog {
        generation: current,
        result,
    } = receive(&mut rx).await
    else {
        panic!("expected failed catalog")
    };
    assert!(matches!(
        apply_session_catalog_result(&mut app, current, result),
        CatalogLoadOutcome::Failed
    ));
    assert!(frame(&mut app, server, &backend).contains("Failed to load sessions"));
    assert!(matches!(
        apply_session_catalog_result(&mut app, generation, Ok(Vec::new())),
        CatalogLoadOutcome::Ignored
    ));
    new_session(&mut app, server, &backend);
    settle(&mut app).await;
    assert_eq!(
        server.registry().list_sessions().await.unwrap().len(),
        2,
        "failed catalog or retry duplicated creation"
    );
    drop(app);
    built.shutdown().await;
}

#[tokio::test]
async fn creation_survives_navigation_without_replay_or_focus_theft() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path(), Arc::new(Probe::default())).await;
    let server = &built.server;
    let backend = BackendManager::new(&None, built.secret_store.clone());
    let mut app = App::hub(HashSet::new());
    let (tx, mut rx) = mpsc::channel(128);
    let (release, gate) = oneshot::channel();
    operations::submit(&mut app, true, None, async move {
        gate.await.unwrap();
        Ok(operations::Effect::Sent)
    });
    new_session(&mut app, server, &backend);
    assert!(app.operations.is_creating_session());
    let generation = app.session_generation;
    app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
    app.navigation_generation += 1; // same ownership transition as the event loop
    app.cancel_session_fill();
    assert!(matches!(
        apply_session_catalog_result(&mut app, generation, Ok(Vec::new())),
        CatalogLoadOutcome::Ignored
    ));
    // View cancellation does not undo or retry an accepted create write.
    release.send(()).unwrap();
    settle(&mut app).await;
    assert_eq!(app.mode, TuiMode::Settings(SettingsScope::Peer));
    assert!(app.tabs.is_empty());
    assert!(!app.operations.is_creating_session());
    let sessions = server.registry().list_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    let created = sessions[0].session_db_id.clone();
    app.close_settings();
    ensure_view(&mut app, server, &tx);
    let SessionLoad::Catalog { generation, result } = receive(&mut rx).await else {
        panic!("expected catalog")
    };
    apply_session_catalog_result(&mut app, generation, result);
    assert_eq!(app.session_list.len(), 1);
    assert_eq!(app.session_list[0].session_db_id, created);
    let (approval, _) = mpsc::channel(8);
    let (notify, _) = mpsc::channel(64);
    dispatch_picker_selection(
        created.clone(),
        &mut app,
        server,
        &backend,
        &approval,
        &notify,
    );
    settle(&mut app).await;
    assert_eq!(app.active().session_db_id, created);
    assert!(app.active().live.observation.is_some());
    assert_eq!(server.registry().list_sessions().await.unwrap().len(), 1);
    drop(app);
    built.shutdown().await;
}

#[tokio::test]
async fn creation_admission_preserves_drafts_and_releases_only_its_own_ticket() {
    let dir = tempfile::tempdir().unwrap();
    let built = runtime(dir.path(), Arc::new(Probe::default())).await;
    let server = &built.server;
    let backend = BackendManager::new(&None, built.secret_store.clone());
    let mut app = App::hub(HashSet::new());
    let (rows, _) = mpsc::channel(128);
    let (release, gate) = oneshot::channel::<()>();
    spawn_session_catalog_load(&mut app, rows.clone(), async move {
        gate.await.unwrap();
        Ok(Vec::new())
    });
    let generation = app.session_generation;
    app.backend_available = false;
    new_session(&mut app, server, &backend);
    assert!(!app.operations.is_creating_session());
    assert_eq!(
        app.session_generation, generation,
        "refusal cancelled independent reads"
    );
    app.backend_available = true;
    for _ in 0..32 {
        operations::submit(&mut app, true, None, std::future::pending());
    }
    new_session(&mut app, server, &backend);
    assert!(!app.operations.is_creating_session());
    assert_eq!(app.session_generation, generation);
    assert_eq!(server.registry().list_sessions().await.unwrap().len(), 0);
    app.operations = operations::Operations::default();
    new_session(&mut app, server, &backend);
    settle(&mut app).await;
    assert_eq!(app.active().current_agent, "default");
    tokio::task::yield_now().await;
    assert!(release.send(()).is_err());
    let (approval, _) = mpsc::channel(8);
    let (notify, _) = mpsc::channel(64);
    let (models, _) = mpsc::channel(4);
    for group in ["unknown", "chosen"] {
        handle_chat_action(
            ChatAction::Dispatch(Command::NewSession(Some(group.into()))),
            &mut app,
            server,
            &backend,
            &built.secret_store,
            &approval,
            &notify,
            &models,
            &rows,
        );
        settle(&mut app).await;
        assert!(
            !app.operations.is_creating_session(),
            "group result stranded creation admission"
        );
        if group == "unknown" {
            assert_eq!(server.registry().list_sessions().await.unwrap().len(), 1);
            assert_eq!(app.active().current_agent, "default");
            assert!(app.operations.indicator().contains("Unknown agent group"));
        } else {
            assert_eq!(server.registry().list_sessions().await.unwrap().len(), 2);
            assert_eq!(app.active().current_agent, "selected");
            assert_eq!(app.active().live.meta.agents[0].display_name, "selected");
            assert!(app.active().live.observation.is_some());
        }
    }
    // A refused slash-command draft must survive just like a full-queue draft.
    let (release, gate) = oneshot::channel();
    operations::submit(&mut app, true, None, async move {
        gate.await.unwrap();
        Ok(operations::Effect::Sent)
    });
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    app.input = "/new chosen".into();
    app.cursor = app.input.len();
    let action = input::handle_chat_key(&mut app, enter).unwrap();
    handle_chat_action(
        action,
        &mut app,
        server,
        &backend,
        &built.secret_store,
        &approval,
        &notify,
        &models,
        &rows,
    );
    assert!(app.operations.is_creating_session());
    app.input = "/new chosen".into();
    app.cursor = 5;
    app.completion_dismissed = true;
    assert!(input::handle_chat_key(&mut app, enter).is_none());
    assert_eq!(app.input, "/new chosen", "refusal discarded the draft");
    assert_eq!(app.cursor, 5);
    assert!(app.completion_dismissed);
    assert!(app.operations.indicator().contains("already pending"));
    release.send(()).unwrap();
    settle(&mut app).await;
    assert_eq!(server.registry().list_sessions().await.unwrap().len(), 3);
    drop(app);
    built.shutdown().await;
}
