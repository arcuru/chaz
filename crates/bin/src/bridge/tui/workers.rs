//! Worker inspection uses only read callbacks and snapshots, never registration.
use super::*;
use chaz_core::session::{
    SessionRegistry,
    worker_links::{self, WorkerEvidence, WorkerSpawn},
};
use futures::{StreamExt, stream};

pub(super) async fn switch_session(identifier: &str, server: &Server) -> CommandOutcome {
    let result = async {
        let id = server
            .registry()
            .find_by_name(identifier)
            .await?
            .unwrap_or_else(|| identifier.to_owned());
        let (conv_id, db) = server.registry().open_session_for_inspection(&id).await?;
        let meta = chaz_core::session::try_read_meta_from_db(&db).await?;
        let agent = server
            .registry()
            .resolve_agent(&id, None, server.agent_index())
            .await;
        Ok::<_, anyhow::Error>(CommandOutcome::SessionSwitched(Box::new(
            commands::SessionSwitch {
                session_db_id: id,
                conv_id,
                db,
                agent_name: agent.name.clone(),
                session_name: meta.name,
            },
        )))
    }
    .await;
    result.unwrap_or_else(|e| CommandOutcome::Error(format!("Failed to switch session: {e}")))
}

pub(super) async fn open_tab(
    server: &Server,
    backend: &BackendManager,
    id: &str,
    tx: mpsc::Sender<String>,
) -> Result<Tab, String> {
    let (_, db) = server
        .registry()
        .open_session_for_inspection(id)
        .await
        .map_err(|e| e.to_string())?;
    if !chaz_core::session::workers::is_worker_session(&db)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err("referenced transcript is not a Worker".into());
    }
    // Install before reading, closing the read/subscribe gap. The existing job
    // callback is nonexecuting and requires the same native materialization floor.
    let observation = jobs::observe(&db, tx).await.map_err(|e| e.to_string())?;
    let mut tab = build_tab(server, backend, db, id.to_owned()).await?;
    tab.live.worker_observer = true;
    tab.live.observation = Some(observation);
    Ok(tab)
}

#[derive(Default)]
pub(super) struct View {
    pub data: Data,
    pub error: Option<String>,
    task: Option<supervised::Task<()>>,
    last: Option<std::time::Instant>,
    generation: u64,
}
impl View {
    pub(super) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.task = None;
        self.last = None;
        self.error = Some("Worker reference binding changed; cached evidence stale".into());
    }
}
#[derive(Default)]
pub(super) struct Data {
    pub evidence: HashMap<String, Result<WorkerEvidence, String>>,
    pub parent: Option<WorkerSpawn>,
    pub parent_error: Option<String>,
}
pub(super) struct Loaded {
    id: String,
    incarnation: u64,
    generation: u64,
    result: Result<Data, String>,
}

/// Only the visible transcript is polled, one coalesced bounded read every five
/// seconds. Outcomes use canonical row references; unfinished attempts also
/// inspect the existing Cancel entries. Hub grouping never reads transcript rows.
pub(super) fn update(app: &mut App, server: &Arc<Server>, tx: &mpsc::Sender<Loaded>) {
    if !app.backend_available || app.mode != TuiMode::Chat {
        return;
    }
    let Some(tab) = app.current_mut() else {
        return;
    };
    let view = &mut tab.live.workers;
    if view.task.is_some()
        || view
            .last
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(5))
    {
        return;
    }
    view.last = Some(std::time::Instant::now());
    let generation = view.generation;
    let id = tab.session_db_id.clone();
    let incarnation = tab.live.incarnation;
    let db = tab.session_db.clone();
    let registry = server.registry_arc();
    let tx = tx.clone();
    view.task = Some(supervised::Task(tokio::spawn(async move {
        let result = supervised::read(async move {
            tokio::time::timeout(std::time::Duration::from_secs(4), load(&registry, &db))
                .await
                .unwrap_or_else(|_| {
                    Err("Worker reference read timed out; cached evidence stale".into())
                })
        })
        .await;
        let _ = tx
            .send(Loaded {
                id,
                incarnation,
                generation,
                result,
            })
            .await;
    })));
}
pub(super) fn apply(app: &mut App, loaded: Loaded) {
    let Some(i) = app.tab_index_for(&loaded.id).filter(|&i| {
        app.tabs[i].live.incarnation == loaded.incarnation
            && app.tabs[i].live.workers.generation == loaded.generation
    }) else {
        return;
    };
    let tab = &mut app.tabs[i];
    tab.live.workers.task = None;
    match loaded.result {
        Ok(data) => {
            tab.live.workers.data = data;
            tab.live.workers.error = None;
        }
        Err(error) => tab.live.workers.error = Some(error),
    }
    tab.touch_transcript();
}
/// Expire cached child activity even while transport is unavailable. A read
/// failure must not freeze a recent heartbeat into indefinitely fresh display.
pub(super) fn age(app: &mut App) -> bool {
    let now = chrono::Utc::now();
    let mut changed = false;
    for tab in &mut app.tabs {
        let mut expired = false;
        for evidence in tab
            .live
            .workers
            .data
            .evidence
            .values_mut()
            .filter_map(|r| r.as_mut().ok())
        {
            if evidence.activity_at.is_some() && evidence.status(now) == "outcome unknown" {
                evidence.activity_at = None;
                evidence.state = "outcome unknown";
                expired = true;
            }
        }
        if expired {
            tab.touch_transcript();
            changed = true;
        }
    }
    changed
}

async fn load(registry: &SessionRegistry, db: &eidetica::Database) -> Result<Data, String> {
    let spawns = worker_links::worker_spawns(db)
        .await
        .map_err(|e| e.to_string())?;
    let parent = async {
        let Some(origin) = worker_links::worker_origin(db).await? else {
            return Ok(None);
        };
        let (_, parent) = registry
            .open_session_for_inspection(&origin.parent_session_id)
            .await?;
        worker_links::worker_parent(db, &parent).await
    }
    .await;
    let evidence = stream::iter(spawns)
        .map(|spawn| async move {
            let result = async {
                let (_, child) = registry
                    .open_session_for_inspection(&spawn.child_session_id)
                    .await?;
                chaz_core::session::jobs::require_observer_write(&child).await?;
                worker_links::worker_evidence(&child, &db.root_id().to_string(), &spawn).await
            }
            .await
            .map_err(|e: anyhow::Error| e.to_string());
            (spawn.spawn_id.clone(), result)
        })
        .buffer_unordered(4)
        .collect()
        .await;
    let (parent, parent_error) = match parent {
        Ok(parent) => (parent, None),
        Err(e) => (None, Some(e.to_string())),
    };
    Ok(Data {
        evidence,
        parent,
        parent_error,
    })
}

#[derive(Default)]
pub(super) struct Groups {
    pub parents: HashMap<String, String>,
    pub notes: HashMap<String, String>,
    pub depths: HashMap<String, usize>,
}
/// Read only small reciprocal side records. No transcript hydration, recursive
/// key acquisition, catalog mirroring, runtime registration or per-frame reads.
pub(super) async fn catalog(
    registry: &SessionRegistry,
    filter: Option<&str>,
) -> Result<(Vec<SessionIndex>, Groups), String> {
    let mut indices = registry.list_sessions().await.map_err(|e| e.to_string())?;
    let worker_indices: Vec<_> = indices
        .iter()
        .filter(|i| chaz_core::session::workers::is_worker_source(i.source.as_deref()))
        .cloned()
        .collect();
    let rows: Vec<_> = stream::iter(worker_indices)
        .map(|index| async move {
            let result = async {
                let (_, child) = registry.open_worker_session(&index.session_db_id).await?;
                let Some(origin) = worker_links::worker_origin(&child).await? else {
                    return Ok(None);
                };
                let (_, parent) = registry
                    .open_session_for_inspection(&origin.parent_session_id)
                    .await?;
                anyhow::ensure!(
                    worker_links::worker_parent(&child, &parent)
                        .await?
                        .is_some(),
                    "Worker relation missing"
                );
                Ok(Some(origin.parent_session_id))
            }
            .await
            .map_err(|e: anyhow::Error| e.to_string());
            (index.session_db_id.clone(), result)
        })
        .buffer_unordered(4)
        .collect()
        .await;
    let mut groups = Groups::default();
    for (id, result) in rows {
        match result {
            Ok(Some(parent)) => {
                groups.parents.insert(id, parent);
            }
            Ok(None) => {
                groups.notes.insert(id, "legacy ancestry unknown".into());
            }
            Err(error) => {
                groups
                    .notes
                    .insert(id, format!("ancestry unavailable: {error}"));
            }
        }
    }
    if let Some(parent_id) = filter {
        let (_, parent) = registry
            .open_session_for_inspection(parent_id)
            .await
            .map_err(|e| e.to_string())?;
        let refs = worker_links::worker_spawns(&parent)
            .await
            .map_err(|e| e.to_string())?;
        let children: HashSet<_> = refs.iter().map(|s| s.child_session_id.clone()).collect();
        indices.retain(|i| children.contains(&i.session_db_id));
        for id in children {
            if !indices.iter().any(|i| i.session_db_id == id) {
                indices.push(SessionIndex {
                    session_db_id: id.clone(),
                    source: Some("spawn:reference".into()),
                    bridge: chaz_core::session::BridgeKind::Spawn,
                    created_at: None,
                    status: chaz_core::session::SessionStatus::Active,
                    name: None,
                });
                groups
                    .notes
                    .insert(id, "referenced transcript unavailable locally".into());
            }
        }
    }
    Ok((indices, groups))
}

/// Order once at a catalog refresh boundary; lazy row metadata cannot reshuffle
/// the forest. Invalid cycles/missing parents remain ordinary navigable roots.
pub(super) fn group(app: &mut App, mut groups: Groups) {
    let selected = app.picker_selection();
    let ids: HashSet<_> = app
        .session_list
        .iter()
        .map(|r| r.session_db_id.clone())
        .collect();
    let mut invalid = HashSet::new();
    for id in &ids {
        let mut seen = HashSet::from([id.as_str()]);
        let mut cursor = id.as_str();
        while let Some(parent) = groups.parents.get(cursor) {
            if !ids.contains(parent) {
                // A filtered view deliberately omits its parent.
                if app.workers_filter.as_deref() != Some(parent) {
                    invalid.insert(id.clone());
                }
                break;
            }
            if !seen.insert(parent) {
                invalid.insert(id.clone());
                break;
            }
            cursor = parent;
        }
    }
    for id in invalid {
        groups.parents.remove(&id);
        groups
            .notes
            .insert(id, "ancestry conflicted or parent unavailable".into());
    }
    let rows = std::mem::take(&mut app.session_list);
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots = Vec::new();
    for row in &rows {
        if let Some(parent) = groups
            .parents
            .get(&row.session_db_id)
            .filter(|p| ids.contains(*p))
        {
            children
                .entry(parent.clone())
                .or_default()
                .push(row.session_db_id.clone());
        } else {
            roots.push(row.session_db_id.clone());
        }
    }
    for list in children.values_mut() {
        list.sort();
    }
    let mut by_id: HashMap<_, _> = rows
        .into_iter()
        .map(|r| (r.session_db_id.clone(), r))
        .collect();
    let mut stack: Vec<_> = roots.into_iter().rev().map(|id| (id, 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if let Some(row) = by_id.remove(&id) {
            groups.depths.insert(id.clone(), depth);
            app.session_list.push(row);
            for child in children.remove(&id).unwrap_or_default().into_iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }
    // Defensive fallback: no session disappears even under malformed metadata.
    let mut remaining: Vec<_> = by_id.into_values().collect();
    remaining.sort_by(|a, b| a.session_db_id.cmp(&b.session_db_id));
    app.session_list.extend(remaining);
    app.picker_index = app
        .session_list
        .iter()
        .position(|r| r.session_db_id == selected)
        .map_or(0, |i| i + 1);
    app.worker_groups = groups;
}

pub(super) fn open_picker(app: &mut App, server: &Arc<Server>, tx: &mpsc::Sender<SessionLoad>) {
    let parent = app.current().map(|t| t.session_db_id.clone());
    enter_session_picker(app, parent);
    refresh_session_catalog(app, server, tx);
}

pub(super) enum Destination {
    Child(WorkerSpawn),
    Parent(WorkerSpawn),
}
pub(super) fn navigate(
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    tx: &mpsc::Sender<String>,
    destination: Destination,
) {
    let Some(source) = app.current().map(|t| t.session_db.clone()) else {
        return;
    };
    let server = server.clone();
    let backend = backend.clone();
    let tx = tx.clone();
    operations::submit(app, false, None, async move {
        let (id, target, call, child) = match destination {
            Destination::Child(spawn) => {
                let (_, child) = server
                    .registry()
                    .open_session_for_inspection(&spawn.child_session_id)
                    .await
                    .map_err(|e| e.to_string())?;
                worker_links::verify_worker_spawn(&source, &child, &spawn)
                    .await
                    .map_err(|e| e.to_string())?;
                if !chaz_core::session::workers::is_worker_session(&child)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    return Err("referenced transcript is not a Worker".into());
                }
                (spawn.child_session_id, child, None, true)
            }
            Destination::Parent(spawn) => {
                let origin = worker_links::worker_origin(&source)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "Worker parent unavailable".to_string())?;
                let (_, parent) = server
                    .registry()
                    .open_session_for_inspection(&origin.parent_session_id)
                    .await
                    .map_err(|e| e.to_string())?;
                if worker_links::worker_parent(&source, &parent)
                    .await
                    .map_err(|e| e.to_string())?
                    .as_ref()
                    != Some(&spawn)
                {
                    return Err("Worker parent relation changed".into());
                }
                worker_links::verify_worker_spawn(&parent, &source, &spawn)
                    .await
                    .map_err(|e| e.to_string())?;
                (origin.parent_session_id, parent, Some(spawn.call), false)
            }
        };
        let observation = jobs::observe(&target, tx)
            .await
            .map_err(|e| e.to_string())?;
        let mut tab = build_tab(&server, &backend, target, id).await?;
        tab.live.observation = Some(observation);
        // Both destinations are observers, including newly opened ordinary
        // parents on executor-capable TUIs. Existing parent tabs keep drafts.
        tab.live.worker_observer = true;
        if child {
            tab.scroll_offset = u16::MAX; // The staged Directive is the initial child row.
        } else if let Some(call) = call {
            tab.live.jump_call = tab
                .live
                .tool_activity
                .items
                .iter()
                .find(|i| i.key.matches_worker(&call))
                .map(|i| i.key.clone());
            if tab.live.jump_call.is_none() {
                tab.live.input_feedback = "Parent opened; exact spawning item unavailable".into();
            }
        }
        Ok(operations::Effect::WorkerNavigation(Box::new(tab)))
    });
}

pub(super) fn focus(tab: &mut Tab, call: tool_activity::CallKey) {
    if tab.expand_all {
        tab.live.expanded_runs.remove(&call.run);
    } else {
        tab.live.expanded_runs.insert(call.run.clone());
    }
    tab.live.jump_call = Some(call);
    tab.touch_transcript();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn worker_read_from_retired_binding_cannot_clear_stale_evidence() {
        let mut app = App::new(
            HashSet::new(),
            super::super::session_picker_tests::test_tab().await,
        );
        let id = app.active().session_db_id.clone();
        let incarnation = app.active().live.incarnation;
        let generation = app.active().live.workers.generation;
        app.active_mut().live.cancel_reads();
        apply(
            &mut app,
            Loaded {
                id,
                incarnation,
                generation,
                result: Ok(Data::default()),
            },
        );
        assert!(
            app.active()
                .live
                .workers
                .error
                .as_ref()
                .unwrap()
                .contains("stale")
        );
        assert_ne!(app.active().live.workers.generation, generation);
    }

    #[test]
    fn grouped_workers_preserve_roots_selection_and_conflicted_or_legacy_rows() {
        let mut app = App::hub(HashSet::new());
        let rows: Vec<_> = [
            "child-b", "root", "child-a", "legacy", "cycle-a", "cycle-b", "orphan",
        ]
        .into_iter()
        .map(|id| SessionIndex {
            session_db_id: id.into(),
            source: Some("spawn:fixture".into()),
            bridge: chaz_core::session::BridgeKind::Spawn,
            created_at: None,
            status: chaz_core::session::SessionStatus::Active,
            name: None,
        })
        .collect();
        apply_session_catalog(&mut app, rows);
        app.picker_index = app
            .session_list
            .iter()
            .position(|r| r.session_db_id == "child-b")
            .unwrap()
            + 1;
        group(
            &mut app,
            Groups {
                parents: [
                    ("child-b", "root"),
                    ("child-a", "root"),
                    ("cycle-a", "cycle-b"),
                    ("cycle-b", "cycle-a"),
                    ("orphan", "absent"),
                ]
                .into_iter()
                .map(|(a, b)| (a.into(), b.into()))
                .collect(),
                ..Default::default()
            },
        );
        assert_eq!(app.picker_selection(), "child-b");
        let ids: Vec<_> = app
            .session_list
            .iter()
            .map(|r| r.session_db_id.as_str())
            .collect();
        let root = ids.iter().position(|id| *id == "root").unwrap();
        assert_eq!(&ids[root..root + 3], &["root", "child-a", "child-b"]);
        assert_eq!(ids.len(), 7);
        assert_eq!(app.worker_groups.depths["child-b"], 1);
        for id in ["cycle-a", "cycle-b", "orphan"] {
            assert!(app.worker_groups.notes[id].contains("conflicted"));
            assert_eq!(app.worker_groups.depths[id], 0);
        }
        assert_eq!(app.worker_groups.depths["legacy"], 0);
    }
}
