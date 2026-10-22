//! Background refresh of backend-owned tab state.
//!
//! Reading a session (entries, meta, live turn claims, extension outputs) is
//! storage I/O that can be slow — over IPC it routinely takes hundreds of
//! milliseconds. None of it may run on the input/render task: the run loop
//! only *requests* a refresh here and keeps drawing the state it already has.
//! The read runs on a spawned task and comes back as a [`Refresh`] message.
//!
//! Each tab owns one [`RefreshSlot`] per kind of read. A slot allows at most
//! one read in flight; a request that arrives while one is running marks the
//! slot dirty, and the completion asks for exactly one follow-up read. A burst
//! of session writes therefore costs at most two reads per tab, and background
//! work stays bounded by `tabs × kinds`.
//!
//! Every read carries a ticket from a single monotonic counter. A completion
//! is applied only when its tab still exists (looked up by session id, never
//! by index) and its ticket is the one the slot is waiting for, so a result
//! for a closed, replaced or superseded request is discarded instead of
//! overwriting newer state or landing on another session.

use super::*;
use chaz_core::session::TurnActivity;

/// One kind of background read for one tab: at most one in flight, plus a
/// dirty bit that coalesces every request made while it runs.
#[derive(Debug, Default)]
pub(super) struct RefreshSlot {
    in_flight: Option<u64>,
    dirty: bool,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for RefreshSlot {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl RefreshSlot {
    /// Claim the slot for `ticket`. Returns `false` (and marks the slot
    /// dirty) when a read is already running; that read's completion then
    /// requests the single follow-up.
    fn begin(&mut self, ticket: u64) -> bool {
        if self.in_flight.is_some() {
            self.dirty = true;
            return false;
        }
        self.in_flight = Some(ticket);
        true
    }

    /// Release the slot for a completed read. `None` means the result is
    /// stale and must be dropped; `Some(follow_up)` says whether a request
    /// arrived meanwhile and another read is needed.
    fn finish(&mut self, ticket: u64) -> Option<bool> {
        if self.in_flight != Some(ticket) {
            return None;
        }
        self.in_flight = None;
        self.task = None;
        Some(std::mem::take(&mut self.dirty))
    }

    #[cfg(test)]
    pub(super) fn is_pending(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// Backend-derived per-tab state plus its refresh bookkeeping. Grouped so a
/// `Tab` literal only needs `live: TabLive::default()`.
#[derive(Default)]
pub(super) struct TabLive {
    pub(super) input_attempt: Option<String>,
    pub(super) inputs: Vec<chaz_core::session::steering::JobInput>,
    pub(super) input_feedback: String,
    pub(super) input_retry:
        Arc<tokio::sync::Mutex<Option<chaz_core::session::steering::JobInputRequest>>>,
    /// Full session reload (entries + meta + activity) after a write.
    pub(super) reload: RefreshSlot,
    /// Extension status strip read.
    pub(super) status: RefreshSlot,
    /// Activity evidence read only on invalidation, initial load or retry.
    pub(super) activity: RefreshSlot,
    pub(super) job: RefreshSlot,
    /// Ticket of the newest read whose `active_turns` has been applied, so a
    /// slow tick can't overwrite a newer reload's count (or vice versa).
    activity_applied: u64,
    pub(super) activity_evidence: Option<TurnActivity>,
    /// Last extension status segments read for this session. Shown as-is
    /// while a refresh is pending; never borrowed from another tab.
    pub(super) status_segments: Vec<String>,
    /// An unresolved binding or per-kind read failure. A successful unrelated
    /// read cannot clear it or make cached data pass for fresh data.
    pub(super) error: Option<String>,
    errors: [Option<String>; 4],
    pub(super) meta: SessionMeta,
    pub(super) telemetry: Option<chaz_core::session::usage::SessionTelemetry>,
    /// View-local command/error output is not stored in the session DB.
    local_entries: Vec<SessionEntry>,
    pub(super) incarnation: u64,
    pub(super) observation: Option<Observation>,
    pub(super) rebind_needed: bool,
    pub(super) unbound: bool,
    pub(super) rebind_ticket: Option<u64>,
    pub(super) rebind_task: Option<supervised::Task<()>>,
    pub(super) binding_error: Option<String>,
    /// Bumped whenever `entries` or `expanded_entries` change, so the
    /// transcript render cache knows when its wrapped lines are stale.
    pub(super) transcript_rev: u64,
    /// Wrapped transcript reused across frames until `transcript_rev`, the
    /// width or a display toggle changes.
    pub(super) render: view::TranscriptCache,
}

impl TabLive {
    pub(super) fn cancel_reads(&mut self) {
        self.reload = RefreshSlot::default();
        self.status = RefreshSlot::default();
        self.activity = RefreshSlot::default();
        self.job = RefreshSlot::default();
        self.rebind_task = None;
        self.rebind_ticket = None;
    }
    pub(super) fn with_meta(
        meta: SessionMeta,
        telemetry: chaz_core::session::usage::SessionTelemetry,
    ) -> Self {
        Self {
            meta,
            telemetry: Some(telemetry),
            ..Default::default()
        }
    }
    pub(super) fn failed(&self, kind: RefreshKind) -> bool {
        self.errors[kind as usize].is_some()
    }
    pub(super) fn pending(&self) -> bool {
        self.reload.in_flight.is_some()
            || self.status.in_flight.is_some()
            || self.activity.in_flight.is_some()
            || self.job.in_flight.is_some()
            || self.rebind_task.is_some()
    }
}

/// What a session reload read from storage.
pub(super) struct SessionSnapshot {
    pub(super) input_attempt: Option<String>,
    pub(super) inputs: Vec<chaz_core::session::steering::JobInput>,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) telemetry: chaz_core::session::usage::SessionTelemetry,
    pub(super) meta: SessionMeta,
    pub(super) active_turns: Result<TurnActivity, String>,
}

/// A completed background read, routed back to the run loop.
pub(super) enum Refresh {
    Job {
        session_db_id: String,
        ticket: u64,
        snapshot: Result<Box<jobs::Snapshot>, String>,
    },
    Session {
        session_db_id: String,
        ticket: u64,
        snapshot: Result<Box<SessionSnapshot>, String>,
    },
    Status {
        session_db_id: String,
        ticket: u64,
        segments: Result<Vec<String>, String>,
    },
    Activity {
        session_db_id: String,
        ticket: u64,
        active_turns: Result<TurnActivity, String>,
    },
}

/// The kinds of read a tab can request; returned as follow-ups so the run
/// loop (not the apply step) decides how to spawn them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RefreshKind {
    Session,
    Status,
    Activity,
    Job,
}

/// Result of applying one [`Refresh`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Applied {
    /// Visible state changed; the next frame must be drawn.
    pub(super) redraw: bool,
    /// A request arrived while this read ran; issue one more.
    pub(super) follow_up: Option<(String, RefreshKind)>,
}

/// Retry failed reads; unchanged ordinary sessions only age retained evidence.
pub(super) fn request_tick(app: &mut App, tx: &mpsc::Sender<Refresh>) -> bool {
    let redraw = age_activity(app, chrono::Utc::now());
    let requests: Vec<_> = app
        .tabs
        .iter()
        .flat_map(|tab| {
            [
                RefreshKind::Session,
                RefreshKind::Status,
                RefreshKind::Activity,
            ]
            .into_iter()
            .filter(|kind| {
                if tab.job.is_some() {
                    // Job polling belongs to the job-observation path.
                    return *kind == RefreshKind::Activity || tab.live.failed(*kind);
                }
                if *kind == RefreshKind::Activity {
                    return tab.live.reload.in_flight.is_none()
                        && (tab.live.activity_evidence.is_none()
                            || tab.live.failed(*kind)
                            // A transcript/meta failure must not prevent a
                            // healthy activity read from observing completion.
                            || tab.live.failed(RefreshKind::Session));
                }
                tab.live.failed(*kind)
            })
            .map(|kind| (tab.session_db_id.clone(), kind))
        })
        .collect();
    for (id, kind) in requests {
        request(app, &id, kind, tx);
    }
    redraw
}

pub(super) fn age_activity(app: &mut App, now: chrono::DateTime<chrono::Utc>) -> bool {
    let mut redraw = false;
    for tab in &mut app.tabs {
        if tab.job.is_none()
            && !tab.live.unbound
            && let Some(evidence) = &tab.live.activity_evidence
        {
            let n = evidence.active_at(now).count();
            redraw |= tab.active_turns != n;
            tab.active_turns = n;
            if tab
                .pending_approval
                .as_ref()
                .is_some_and(|ex| ex.decision_tx.is_closed())
            {
                tab.pending_approval = None;
                redraw = true;
            }
        }
    }
    redraw
}

impl App {
    fn next_ticket(&mut self) -> u64 {
        self.refresh_ticket += 1;
        self.refresh_ticket
    }
}

impl Tab {
    /// Record a change to the transcript source (`entries` /
    /// `expanded_entries`) for the render cache.
    pub(super) fn touch_transcript(&mut self) {
        self.live.transcript_rev = self.live.transcript_rev.wrapping_add(1);
    }

    pub(super) fn set_entries(&mut self, mut entries: Vec<SessionEntry>) {
        entries.extend(self.live.local_entries.iter().cloned());
        // Stable sort preserves backend row order on timestamp ties while
        // retaining local command output between subsequent persisted rows.
        entries.sort_by_key(|e| e.timestamp);
        self.entries = entries;
        self.live.telemetry = None;
        self.touch_transcript();
    }

    pub(super) fn add_local(&mut self, entry: SessionEntry) {
        self.live.local_entries.push(entry.clone());
        self.entries.push(entry);
        self.touch_transcript();
    }

    pub(super) fn clear_view(&mut self) {
        self.entries.clear();
        self.live.telemetry = None;
        self.live.local_entries.clear();
        self.scroll_offset = 0;
        self.expanded_entries.clear();
        self.touch_transcript();
    }
}

/// Spawn `load` for tab `session_db_id`'s `kind` slot unless one is already
/// running (then the request is coalesced into a single follow-up). Never
/// awaits: the caller returns to input/render immediately.
pub(super) fn request_with<Fut>(
    app: &mut App,
    session_db_id: &str,
    kind: RefreshKind,
    tx: &mpsc::Sender<Refresh>,
    load: impl FnOnce(u64) -> Fut + Send + 'static,
) where
    Fut: Future<Output = Refresh> + Send + 'static,
{
    let Some(idx) = app.tab_index_for(session_db_id) else {
        return;
    };
    let ticket = app.next_ticket();
    let live = &mut app.tabs[idx].live;
    let slot = match kind {
        RefreshKind::Session => &mut live.reload,
        RefreshKind::Status => &mut live.status,
        RefreshKind::Activity => &mut live.activity,
        RefreshKind::Job => &mut live.job,
    };
    if !slot.begin(ticket) {
        return;
    }
    let tx = tx.clone();
    let id = session_db_id.to_owned();
    slot.task = Some(tokio::spawn(async move {
        let msg = supervised::read(async move { Ok(load(ticket).await) })
            .await
            .unwrap_or_else(|e| match kind {
                RefreshKind::Job => Refresh::Job {
                    session_db_id: id,
                    ticket,
                    snapshot: Err(e),
                },
                RefreshKind::Session => Refresh::Session {
                    session_db_id: id,
                    ticket,
                    snapshot: Err(e),
                },
                RefreshKind::Status => Refresh::Status {
                    session_db_id: id,
                    ticket,
                    segments: Err(e),
                },
                RefreshKind::Activity => Refresh::Activity {
                    session_db_id: id,
                    ticket,
                    active_turns: Err(e),
                },
            });
        let _ = tx.send(msg).await;
    }));
}

/// Request a background read of `kind` for the tab hosting `session_db_id`
/// using the real session store.
pub(super) fn request(
    app: &mut App,
    session_db_id: &str,
    kind: RefreshKind,
    tx: &mpsc::Sender<Refresh>,
) {
    let Some(idx) = app.tab_index_for(session_db_id) else {
        return;
    };
    if app.tabs[idx].live.unbound {
        return;
    }
    let kind = if app.tabs[idx].job.is_some() {
        RefreshKind::Job
    } else {
        kind
    };
    let db = app.tabs[idx].session_db.clone();
    let id = session_db_id.to_string();
    request_with(app, session_db_id, kind, tx, move |ticket| async move {
        match kind {
            RefreshKind::Job => Refresh::Job {
                snapshot: tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    jobs::snapshot(&db),
                )
                .await
                .unwrap_or_else(|_| Err("refresh timed out; retained evidence is stale".into()))
                .map(Box::new),
                session_db_id: id,
                ticket,
            },
            RefreshKind::Session => Refresh::Session {
                snapshot: load_session_snapshot(&id, db).await.map(Box::new),
                session_db_id: id,
                ticket,
            },
            RefreshKind::Status => Refresh::Status {
                segments: load_status_segments(&db).await,
                session_db_id: id,
                ticket,
            },
            RefreshKind::Activity => Refresh::Activity {
                active_turns: load_active_turns(&db).await,
                session_db_id: id,
                ticket,
            },
        }
    });
}

pub(super) async fn load_session_snapshot(
    _session_db_id: &str,
    db: eidetica::Database,
) -> Result<SessionSnapshot, String> {
    async {
        let snapshot = db.snapshot().await?;
        let entries = Session::entries_at_snapshot(&db, &snapshot).await?;
        let telemetry =
            chaz_core::session::usage::telemetry_at_snapshot(&db, &snapshot, &entries).await?;
        let meta = chaz_core::session::try_read_meta_from_db(&db).await?;
        let session = Session::try_new(
            chaz_core::types::ConversationId(db.root_id().to_string()),
            db.clone(),
        )
        .await?;
        let attempts = Session::steering_attempts_at(&db, &snapshot).await?;
        Ok::<_, anyhow::Error>(SessionSnapshot {
            input_attempt: match attempts.as_slice() {
                [id] => Some(id.clone()),
                _ => None,
            },
            inputs: session.turn_inputs_at(&snapshot).await?,
            entries,
            telemetry,
            meta,
            active_turns: load_active_turns(&db).await,
        })
    }
    .await
    .map_err(|e| e.to_string())
}

pub(super) async fn load_active_turns(db: &eidetica::Database) -> Result<TurnActivity, String> {
    Session::turn_activity(db).await.map_err(|e| e.to_string())
}

/// Flatten every extension's status map from the session's
/// `extension_outputs` store (written by the daemon at the turn boundary)
/// into render-ready segments, ordered by extension name then key.
pub(super) async fn load_status_segments(db: &eidetica::Database) -> Result<Vec<String>, String> {
    chaz_core::extension::try_read_extension_outputs(db)
        .await
        .map(|outputs| {
            outputs
                .into_values()
                .flat_map(|o| o.status.into_values())
                .collect()
        })
        .map_err(|e| e.to_string())
}

fn record_error(tab: &mut Tab, kind: RefreshKind, error: Option<String>) {
    if let Some(e) = &error {
        tracing::warn!(session = %tab.session_db_id, ?kind, "Refresh failed: {e}");
    }
    tab.live.errors[kind as usize] = error;
    sync_error(tab);
}
pub(super) fn sync_error(tab: &mut Tab) {
    tab.live.error = tab
        .live
        .binding_error
        .clone()
        .or_else(|| tab.live.errors.iter().find_map(Clone::clone));
}

fn apply_active_turns(
    tab: &mut Tab,
    ticket: u64,
    active_turns: Result<TurnActivity, String>,
) -> bool {
    if ticket < tab.live.activity_applied {
        return false;
    }
    tab.live.activity_applied = ticket;
    match active_turns {
        Ok(evidence) => {
            let n = evidence.active_at(chrono::Utc::now()).count();
            tab.live.activity_evidence = Some(evidence);
            let approval_closed = tab
                .pending_approval
                .as_ref()
                .is_some_and(|ex| ex.decision_tx.is_closed());
            if approval_closed {
                tab.pending_approval = None;
            }
            let changed = tab.active_turns != n || tab.live.error.is_some() || approval_closed;
            tab.active_turns = n;
            record_error(tab, RefreshKind::Activity, None);
            changed
        }
        Err(e) => {
            tracing::warn!(session = %tab.session_db_id, "Live turn refresh failed: {e}");
            record_error(tab, RefreshKind::Activity, Some(e));
            true
        }
    }
}

/// Apply a completed read to its tab. Stale or orphaned results are dropped.
/// Synchronous: everything here is in-memory.
pub(super) fn apply(
    app: &mut App,
    msg: Refresh,
    resolve: impl FnOnce(&str, &SessionMeta) -> Resolved,
) -> Applied {
    let (session_db_id, ticket, kind) = match &msg {
        Refresh::Job {
            session_db_id,
            ticket,
            ..
        } => (session_db_id.clone(), *ticket, RefreshKind::Job),
        Refresh::Session {
            session_db_id,
            ticket,
            ..
        } => (session_db_id.clone(), *ticket, RefreshKind::Session),
        Refresh::Status {
            session_db_id,
            ticket,
            ..
        } => (session_db_id.clone(), *ticket, RefreshKind::Status),
        Refresh::Activity {
            session_db_id,
            ticket,
            ..
        } => (session_db_id.clone(), *ticket, RefreshKind::Activity),
    };
    let Some(idx) = app.tab_index_for(&session_db_id) else {
        return Applied::default();
    };
    let is_active = idx == app.active_tab;
    let live = &mut app.tabs[idx].live;
    let slot = match kind {
        RefreshKind::Session => &mut live.reload,
        RefreshKind::Status => &mut live.status,
        RefreshKind::Activity => &mut live.activity,
        RefreshKind::Job => &mut live.job,
    };
    let Some(again) = slot.finish(ticket) else {
        return Applied::default();
    };
    let follow_up = again.then(|| (session_db_id.clone(), kind));

    let redraw = match msg {
        Refresh::Job { snapshot, .. } => {
            let tab = &mut app.tabs[idx];
            record_error(tab, kind, snapshot.as_ref().err().cloned());
            jobs::apply_snapshot(tab, snapshot.map(|snapshot| *snapshot));
            true
        }
        Refresh::Status { segments, .. } => {
            let tab = &mut app.tabs[idx];
            replace_status(tab, segments);
            is_active
        }
        Refresh::Activity { active_turns, .. } => {
            apply_active_turns(&mut app.tabs[idx], ticket, active_turns) && is_active
        }
        Refresh::Session { snapshot, .. } => {
            match snapshot {
                Ok(snapshot) => {
                    record_error(&mut app.tabs[idx], kind, None);
                    apply_session_snapshot(app, idx, ticket, *snapshot, resolve);
                }
                Err(e) => record_error(&mut app.tabs[idx], kind, Some(e)),
            }
            // The tab title and picker rows are visible from every tab.
            true
        }
    };
    Applied { redraw, follow_up }
}

/// Tab display values derived from session meta plus in-memory server
/// config (agents, backends). No I/O; injected into [`apply`] so tests can
/// drive it without a server.
pub(super) struct Resolved {
    pub(super) effective_model: String,
    pub(super) context_budget: usize,
    pub(super) roster: Vec<RosterAgent>,
}

/// Resolve the tab's display values from fresh meta. If `/model X` or
/// `/model <agent> Y` ran on this session (or a remote peer pinned a model),
/// the effective model moves; the per-agent override beats the session pin
/// for the tab's current agent. The budget follows the model, and the full
/// roster follows attach/detach, host changes and per-agent pins.
pub(super) fn resolve(
    server: &Server,
    backend: &BackendManager,
    current_agent: &str,
    meta: &SessionMeta,
) -> Resolved {
    let agent = server.agents().get(current_agent);
    let agent_default = agent.as_ref().and_then(|a| a.default_model.clone());
    let session_model = meta
        .resolve_model_for_agent(current_agent)
        .map(str::to_string);
    let effective_model =
        backend.resolve_model_name(session_model.as_deref().or(agent_default.as_deref()));
    let agent_cap = agent.as_ref().and_then(|a| a.max_context_tokens);
    let context_budget = server.effective_context_budget(&effective_model, agent_cap);
    Resolved {
        effective_model,
        context_budget,
        roster: build_roster(server, backend, meta),
    }
}

pub(super) fn replace_status(tab: &mut Tab, status: Result<Vec<String>, String>) {
    match status {
        Ok(status) => {
            tab.live.status_segments = status;
            record_error(tab, RefreshKind::Status, None);
        }
        Err(error) => record_error(tab, RefreshKind::Status, Some(error)),
    }
}

fn apply_session_snapshot(
    app: &mut App,
    idx: usize,
    ticket: u64,
    snapshot: SessionSnapshot,
    resolve: impl FnOnce(&str, &SessionMeta) -> Resolved,
) {
    let SessionSnapshot {
        input_attempt,
        inputs,
        entries,
        telemetry,
        meta,
        active_turns,
    } = snapshot;
    let Resolved {
        effective_model,
        context_budget,
        roster,
    } = resolve(&app.tabs[idx].current_agent, &meta);

    let tab = &mut app.tabs[idx];
    tab.live.input_attempt = input_attempt;
    tab.live.inputs = inputs;
    tab.set_entries(entries);
    tab.live.telemetry = Some(telemetry);
    tab.session_name = meta.name.clone();
    tab.model_pin = meta
        .resolve_model_for_agent(&tab.current_agent)
        .map(str::to_owned);
    tab.live.meta = meta.clone();
    tab.effective_model = effective_model;
    tab.context_budget = context_budget;
    tab.roster = roster;
    apply_active_turns(tab, ticket, active_turns);
    let db_id = tab.session_db_id.clone();
    let entry_count = tab.entries.len();

    // If Settings(Session) is up on the same session, refresh the snapshot
    // from the meta just read so edits (model pin, agent attach/detach)
    // propagate immediately. `created_at` is registry data a session write
    // can't change, so the existing value is kept.
    if matches!(app.mode, TuiMode::Settings(SettingsScope::Session))
        && let Some(s) = app
            .session_settings_snapshot
            .as_mut()
            .filter(|s| s.session_db_id == db_id)
    {
        s.model_pin = meta.model.clone();
        s.agent_models = meta.agent_models.clone();
        s.agents = meta.agents.clone();
        s.host_agent_db_id = meta.host_agent_db_id.clone();
        s.entry_count = entry_count;
    }

    if let Some(index) = app.session_indices.get_mut(&db_id) {
        index.name = meta.name.clone();
    }

    // Keep mutable row metadata in lock-step without deriving transcript
    // summaries for the picker.
    if let Some(row) = app
        .session_list
        .iter_mut()
        .find(|s| s.session_db_id == db_id)
    {
        row.name = meta.name.clone();
        row.agent_name = meta.agent_name.clone();
        row.loaded = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chaz_core::session::TurnActivity;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewAccount};
    use ratatui::backend::TestBackend;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// App with one tab per id. Every tab shares one live in-memory DB:
    /// loaders are injected, so the handle is never read.
    pub(super) async fn app_with_tabs(ids: &[&str]) -> (Instance, App) {
        let (instance, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::passwordless("tab-refresh"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        let tabs: Vec<Tab> = ids.iter().map(|id| tab(id, db.clone())).collect();
        let mut tabs = tabs.into_iter();
        let mut app = App::new(HashSet::new(), tabs.next().unwrap());
        app.tabs.extend(tabs);
        (instance, app)
    }

    pub(super) fn tab(id: &str, db: eidetica::Database) -> Tab {
        Tab {
            job: None,
            session_db_id: id.to_string(),
            session_db: db,
            entries: Vec::new(),
            scroll_offset: 0,
            pending_approval: None,
            active_turns: 0,
            current_agent: "chaz".into(),
            model_pin: None,
            session_name: None,
            effective_model: String::new(),
            roster: Vec::new(),
            context_budget: 0,
            expanded_entries: HashSet::new(),
            live: TabLive::default(),
        }
    }

    fn message(content: &str) -> SessionEntry {
        SessionEntry {
            sender: "user".into(),
            content: content.into(),
            timestamp: chrono::Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        }
    }

    pub(super) fn resolved(_: &str, _: &SessionMeta) -> Resolved {
        Resolved {
            effective_model: "test-model".into(),
            context_budget: 0,
            roster: Vec::new(),
        }
    }

    fn status(id: &str, ticket: u64, segment: &str) -> Refresh {
        Refresh::Status {
            session_db_id: id.into(),
            ticket,
            segments: Ok(vec![segment.into()]),
        }
    }

    fn snapshot(id: &str, ticket: u64, entries: Vec<SessionEntry>, turns: TurnActivity) -> Refresh {
        Refresh::Session {
            session_db_id: id.into(),
            ticket,
            snapshot: Ok(Box::new(SessionSnapshot {
                input_attempt: None,
                inputs: vec![],
                telemetry: chaz_core::session::usage::SessionTelemetry::from_entries(&entries),
                entries,
                meta: SessionMeta {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
                active_turns: Ok(turns),
            })),
        }
    }

    #[tokio::test]
    async fn per_call_telemetry_refresh_renders_alias_missing_usage_and_multi_agent_totals() {
        use chaz_core::runtime::{ResponseMetadata, TokenUsage};
        use chaz_core::session::{
            TurnCallIdentity, TurnRequestId, TurnTranscriptMessage, TurnTranscriptRecord,
        };
        let (_instance, mut app) = app_with_tabs(&["a"]).await;
        let db = app.active().session_db.clone();
        let session = Session::new(chaz_core::types::ConversationId("a".into()), db).await;
        let (tx, mut rx) = mpsc::channel(8);
        for (sequence, agent, prompt) in [
            (0, "chaz", Some(1000)),
            (1, "guest", Some(9000)),
            (2, "chaz", Some(2000)),
            (3, "chaz", None),
            (4, "chaz", Some(0)),
        ] {
            session
                .append_turn_transcript(
                    TurnTranscriptRecord {
                        request_id: TurnRequestId::parse("fixture-request"),
                        attempt_id: "fixture-attempt".into(),
                        sequence,
                        timestamp: chrono::Utc::now(),
                        identity: Some(TurnCallIdentity {
                            agent: agent.into(),
                            requested_model: "gpt-sol-latest".into(),
                            execution_id: None,
                        }),
                        message: TurnTranscriptMessage::ModelResponse {
                            model_sequence: sequence,
                            content: None,
                            tool_calls: Vec::new(),
                            provider_extra: Default::default(),
                            metadata: prompt.map(|prompt| ResponseMetadata {
                                model: "gpt-6.1-sol".into(),
                                usage: TokenUsage {
                                    prompt_tokens: prompt,
                                    completion_tokens: 100,
                                    cached_tokens: Some(prompt / 2),
                                    cost_usd: Some(0.01),
                                    ..Default::default()
                                },
                                ..Default::default()
                            }),
                            terminal: false,
                        },
                    },
                    Vec::new(),
                )
                .await
                .unwrap();
            request(&mut app, "a", RefreshKind::Session, &tx);
            let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let applied = apply(&mut app, msg, |_, _| Resolved {
                effective_model: "gpt-sol-latest".into(),
                context_budget: 10_000,
                roster: Vec::new(),
            });
            assert!(applied.redraw, "background telemetry must request a frame");
            let screen = draw_chat(&mut app, 180, 20).join("\n");
            let expected = match sequence {
                0 | 1 => "ctx 1k/10k (10.0%) tok",
                2 => "ctx 2k/10k (20.0%) tok",
                _ => "ctx unknown/10k tok",
            };
            assert!(screen.contains(expected), "{screen}");
            if sequence == 2 {
                assert!(
                    screen.contains("12k/300 tok • 50% cached • $0.0300"),
                    "{screen}"
                );
                request_with(
                    &mut app,
                    "a",
                    RefreshKind::Session,
                    &tx,
                    |ticket| async move {
                        Refresh::Session {
                            session_db_id: "a".into(),
                            ticket,
                            snapshot: Err("controlled telemetry read failure".into()),
                        }
                    },
                );
                let failed = rx.recv().await.unwrap();
                assert!(apply(&mut app, failed, resolved).redraw);
                let cached = draw_chat(&mut app, 180, 20).join("\n");
                assert!(cached.contains("refresh failed"), "{cached}");
                assert!(cached.contains(expected), "{cached}");
                assert!(
                    cached.contains("12k/300 tok • 50% cached • $0.0300"),
                    "{cached}"
                );
                app.active_mut().effective_model = "other-model".into();
                assert!(
                    draw_chat(&mut app, 180, 20)
                        .join("\n")
                        .contains("ctx unknown/10k tok")
                );
            }
        }
        // Reopening/reconnecting reads the durable telemetry, not runtime memory.
        let reopened = load_session_snapshot("a", app.active().session_db.clone())
            .await
            .unwrap();
        assert_eq!(reopened.telemetry.totals.prompt_tokens, 12_000);
        assert_eq!(reopened.telemetry.last_calls["chaz"].context_tokens, None);
        assert_eq!(
            reopened.telemetry.last_calls["guest"].context_tokens,
            Some(9000)
        );
    }

    /// Request a read whose result is held until the returned sender fires.
    fn request_gated(
        app: &mut App,
        id: &str,
        kind: RefreshKind,
        tx: &mpsc::Sender<Refresh>,
        result: impl FnOnce(u64) -> Refresh + Send + 'static,
    ) -> oneshot::Sender<()> {
        let (release, gate) = oneshot::channel::<()>();
        request_with(app, id, kind, tx, move |ticket| async move {
            let _ = gate.await;
            result(ticket)
        });
        release
    }

    fn draw_chat(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        let segments = app.active().live.status_segments.clone();
        terminal.draw(|f| view::ui_chat(f, app, &segments)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The regression this module exists for: with a status read and a
    /// session reload both stalled on the backend, typing, scrolling and
    /// drawing still complete and show the cached state. Paused time makes
    /// any accidental await on the stalled reads trip the timeout instead of
    /// hanging.
    #[tokio::test(start_paused = true)]
    async fn stalled_reads_leave_typing_scrolling_and_drawing_live() {
        let (_instance, mut app) = app_with_tabs(&["a"]).await;
        app.active_mut().live.status_segments = vec!["cached-status".into()];
        app.active_mut()
            .set_entries((0..200).map(|i| message(&format!("line {i}"))).collect());
        let (tx, mut rx) = mpsc::channel(8);
        let release_status = request_gated(&mut app, "a", RefreshKind::Status, &tx, |t| {
            status("a", t, "fresh-status")
        });
        let session = Session::new(
            chaz_core::types::ConversationId("a".into()),
            app.active().session_db.clone(),
        )
        .await;
        session
            .start_turn_attempt(chaz_core::session::TurnRequestId::parse("live"))
            .await
            .unwrap();
        let activity = load_active_turns(session.database()).await.unwrap();
        let release_reload = request_gated(&mut app, "a", RefreshKind::Session, &tx, move |t| {
            snapshot("a", t, vec![message("reloaded")], activity)
        });
        assert!(app.active().live.status.is_pending());
        assert!(app.active().live.reload.is_pending());

        let frame = tokio::time::timeout(Duration::from_secs(1), async {
            for c in "hello".chars() {
                input::handle_chat_key(&mut app, key(KeyCode::Char(c)));
            }
            input::handle_chat_key(&mut app, key(KeyCode::PageUp));
            draw_chat(&mut app, 60, 20)
        })
        .await
        .expect("input/render waited on a stalled backend read");

        assert_eq!(app.input, "hello");
        assert_eq!(app.active().scroll_offset, 20);
        let text = frame.join("\n");
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("cached-status"), "{text}");
        // Each message is four rows, so 20 rows up hides the last five.
        assert!(text.contains("line 194"), "scrolled view: {text}");
        assert!(!text.contains("line 195"), "scrolled view: {text}");
        assert!(rx.try_recv().is_err(), "reads are still stalled");

        release_status.send(()).unwrap();
        release_reload.send(()).unwrap();
        for _ in 0..2 {
            let msg = rx.recv().await.unwrap();
            let applied = apply(&mut app, msg, resolved);
            assert!(applied.redraw);
            assert_eq!(applied.follow_up, None);
        }
        let tab = app.active();
        assert_eq!(tab.live.status_segments, vec!["fresh-status".to_string()]);
        assert_eq!(tab.entries.len(), 1);
        assert_eq!(tab.session_name.as_deref(), Some("renamed"));
        assert_eq!(tab.effective_model, "test-model");
        assert_eq!(tab.active_turns, 1);
        assert!(!tab.live.reload.is_pending() && !tab.live.status.is_pending());
        // The composer survived the reload untouched.
        assert_eq!(app.input, "hello");
    }

    /// A burst of session writes while a reload runs spawns no extra reads:
    /// it collapses into exactly one follow-up.
    #[tokio::test]
    async fn session_change_burst_coalesces_into_one_follow_up() {
        let (_instance, mut app) = app_with_tabs(&["a"]).await;
        let (tx, mut rx) = mpsc::channel(8);
        let spawned = Arc::new(AtomicUsize::new(0));
        let mut releases = Vec::new();
        for _ in 0..10 {
            let spawned = spawned.clone();
            let (release, gate) = oneshot::channel::<()>();
            releases.push(release);
            request_with(&mut app, "a", RefreshKind::Session, &tx, move |t| {
                spawned.fetch_add(1, Ordering::SeqCst);
                async move {
                    let _ = gate.await;
                    snapshot("a", t, vec![message("first")], TurnActivity::default())
                }
            });
        }
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(spawned.load(Ordering::SeqCst), 1);
        releases.remove(0).send(()).unwrap();
        let applied = apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert_eq!(
            applied.follow_up,
            Some(("a".to_string(), RefreshKind::Session))
        );

        let release = request_gated(&mut app, "a", RefreshKind::Session, &tx, |t| {
            snapshot(
                "a",
                t,
                vec![message("first"), message("second")],
                TurnActivity::default(),
            )
        });
        release.send(()).unwrap();
        let applied = apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert_eq!(
            applied.follow_up, None,
            "nothing arrived during the follow-up"
        );
        assert_eq!(
            app.active().entries.len(),
            2,
            "the follow-up saw the last write"
        );
    }

    /// Late results never overwrite newer state or land on another session:
    /// superseded tickets, closed tabs and a closed-then-reopened session are
    /// all discarded.
    #[tokio::test]
    async fn stale_orphaned_and_cross_session_results_are_dropped() {
        let (_instance, mut app) = app_with_tabs(&["a", "b"]).await;
        let (tx, mut rx) = mpsc::channel(8);

        // Unknown ticket: never requested, or already superseded.
        app.tabs[0].live.status_segments = vec!["kept".into()];
        assert_eq!(
            apply(&mut app, status("a", 999, "stale"), resolved),
            Applied::default()
        );
        assert_eq!(app.tabs[0].live.status_segments, vec!["kept".to_string()]);

        // A background tab's status lands on that tab only, without a redraw.
        let release = request_gated(&mut app, "b", RefreshKind::Status, &tx, |t| {
            status("b", t, "b-status")
        });
        release.send(()).unwrap();
        let applied = apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert!(!applied.redraw, "tab b is not visible");
        assert_eq!(
            app.tabs[1].live.status_segments,
            vec!["b-status".to_string()]
        );
        assert_eq!(app.tabs[0].live.status_segments, vec!["kept".to_string()]);

        // Closed tab: its reload is discarded and touches nothing else.
        let release = request_gated(&mut app, "b", RefreshKind::Session, &tx, |t| {
            snapshot("b", t, vec![message("ghost")], TurnActivity::default())
        });
        release.send(()).unwrap();
        // A result already queued before close still needs the identity guard.
        let late = rx.recv().await.unwrap();
        app.close_tab_at(1);
        assert_eq!(apply(&mut app, late, resolved), Applied::default());
        assert!(app.tabs[0].entries.is_empty());

        // Closed then reopened: the new tab never asked for the old read.
        let release = request_gated(&mut app, "a", RefreshKind::Session, &tx, |t| {
            snapshot("a", t, vec![message("old tab")], TurnActivity::default())
        });
        release.send(()).unwrap();
        let late = rx.recv().await.unwrap();
        let db = app.tabs[0].session_db.clone();
        app.tabs.push(tab("z", db.clone()));
        app.close_tab_at(0);
        app.tabs.push(tab("a", db));
        assert_eq!(apply(&mut app, late, resolved), Applied::default());
        let reopened = &app.tabs[app.tab_index_for("a").unwrap()];
        assert!(reopened.entries.is_empty());
        assert_eq!(reopened.session_name, None);
    }

    /// Refresh work belongs to the tab, not the runtime: closing it or
    /// dropping the app must release even a permanently stalled read.
    #[tokio::test]
    async fn closing_tabs_and_exiting_cancel_pending_reads() {
        let (_instance, mut app) = app_with_tabs(&["a", "b"]).await;
        let (tx, _rx) = mpsc::channel(8);
        let closed = request_gated(&mut app, "a", RefreshKind::Session, &tx, |t| {
            snapshot("a", t, Vec::new(), TurnActivity::default())
        });
        let exiting = request_gated(&mut app, "b", RefreshKind::Status, &tx, |t| {
            status("b", t, "unused")
        });
        tokio::task::yield_now().await;
        assert!(!closed.is_closed() && !exiting.is_closed());

        app.close_tab_at(0);
        tokio::task::yield_now().await;
        assert!(closed.is_closed(), "closed tab left its read running");
        assert!(!exiting.is_closed(), "another tab's read was cancelled");

        drop(app);
        tokio::task::yield_now().await;
        assert!(exiting.is_closed(), "TUI exit left its read running");
    }

    /// A slow activity poll that completes after a newer reload must not roll
    /// the live-turn count back, and a failed read is surfaced, not hidden.
    #[tokio::test]
    async fn older_activity_poll_cannot_roll_back_a_newer_reload_and_errors_show() {
        let (_instance, mut app) = app_with_tabs(&["a"]).await;
        let (tx, mut rx) = mpsc::channel(8);
        let session = Session::new(
            chaz_core::types::ConversationId("a".into()),
            app.active().session_db.clone(),
        )
        .await;
        session
            .start_turn_attempt(chaz_core::session::TurnRequestId::parse("live"))
            .await
            .unwrap();
        let activity = load_active_turns(session.database()).await.unwrap();
        let poll = request_gated(&mut app, "a", RefreshKind::Activity, &tx, move |t| {
            Refresh::Activity {
                session_db_id: "a".into(),
                ticket: t,
                active_turns: Ok(activity),
            }
        });
        let reload = request_gated(&mut app, "a", RefreshKind::Session, &tx, |t| {
            snapshot("a", t, Vec::new(), TurnActivity::default())
        });
        reload.send(()).unwrap();
        apply(&mut app, rx.recv().await.unwrap(), resolved);
        poll.send(()).unwrap();
        apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert_eq!(app.active().active_turns, 0, "older poll was ignored");

        let failing = request_gated(&mut app, "a", RefreshKind::Activity, &tx, |t| {
            Refresh::Activity {
                session_db_id: "a".into(),
                ticket: t,
                active_turns: Err("backend unreachable".into()),
            }
        });
        failing.send(()).unwrap();
        assert!(apply(&mut app, rx.recv().await.unwrap(), resolved).redraw);
        assert!(
            draw_chat(&mut app, 100, 10)
                .join("\n")
                .contains("refresh failed")
        );

        let recovered = request_gated(&mut app, "a", RefreshKind::Activity, &tx, |t| {
            Refresh::Activity {
                session_db_id: "a".into(),
                ticket: t,
                active_turns: Ok(TurnActivity::default()),
            }
        });
        recovered.send(()).unwrap();
        apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert!(app.active().live.error.is_none());
        assert!(
            !draw_chat(&mut app, 100, 10)
                .join("\n")
                .contains("refresh failed")
        );
    }

    /// Switching tabs shows the destination tab's own cached strip at once,
    /// never the previous tab's.
    #[tokio::test]
    async fn tab_switch_shows_that_tabs_cached_status() {
        let (_instance, mut app) = app_with_tabs(&["a", "b"]).await;
        app.tabs[0].live.status_segments = vec!["status-of-a".into()];
        app.tabs[1].live.status_segments = vec!["status-of-b".into()];
        assert!(
            draw_chat(&mut app, 80, 10)
                .join("\n")
                .contains("status-of-a")
        );
        cycle_tab(&mut app, 1);
        let frame = draw_chat(&mut app, 80, 10).join("\n");
        assert!(frame.contains("status-of-b") && !frame.contains("status-of-a"));
    }
    #[tokio::test(start_paused = true)]
    async fn stalled_and_panicked_slots_complete_with_errors_and_recover() {
        let (_instance, mut app) = app_with_tabs(&["a"]).await;
        app.active_mut().live.status_segments = vec!["last-good".into()];
        let (tx, mut rx) = mpsc::channel(8);
        request_with(&mut app, "a", RefreshKind::Status, &tx, |_| {
            std::future::pending()
        });
        let msg = tokio::time::timeout(supervised::READ_TIMEOUT * 2, rx.recv())
            .await
            .expect("stalled read stranded its slot")
            .unwrap(); // paused clock advances to read timeout
        apply(&mut app, msg, resolved);
        assert!(!app.active().live.status.is_pending());
        assert_eq!(app.active().live.status_segments, ["last-good"]);
        assert!(
            app.active()
                .live
                .error
                .as_ref()
                .unwrap()
                .contains("timed out")
        );
        request_with(
            &mut app,
            "a",
            RefreshKind::Status,
            &tx,
            |_| -> std::future::Pending<Refresh> { panic!("fixture refresh constructor panic") },
        );
        apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert!(!app.active().live.status.is_pending());
        assert!(
            app.active()
                .live
                .error
                .as_ref()
                .unwrap()
                .contains("panicked")
        );
        request_with(
            &mut app,
            "a",
            RefreshKind::Activity,
            &tx,
            |ticket| async move {
                Refresh::Activity {
                    session_db_id: "a".into(),
                    ticket,
                    active_turns: Ok(TurnActivity::default()),
                }
            },
        );
        apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert!(
            app.active().live.error.is_some(),
            "activity cannot clear a failed status read"
        );
        request_with(
            &mut app,
            "a",
            RefreshKind::Status,
            &tx,
            |ticket| async move { status("a", ticket, "recovered") },
        );
        apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert_eq!(app.active().live.status_segments, ["recovered"]);
        assert!(app.active().live.error.is_none());
    }

    #[tokio::test]
    async fn actual_service_loss_retains_last_good_transcript_meta_status_and_recovers() {
        use chaz_core::{
            config::{EideticaConfig, EideticaLoginConfig, ExecutionRole},
            extension::{ExtensionOutput, write_extension_outputs},
        };
        use eidetica::service::ServiceServer;
        use std::collections::BTreeMap;
        use tokio::sync::watch;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fixture.sock");
        let (owner, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::passwordless("failure-fixture"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        let id = db.root_id().clone();
        let mut session =
            Session::new(chaz_core::types::ConversationId(id.to_string()), db.clone()).await;
        session
            .add_entry(message("last-good-transcript"))
            .await
            .unwrap();
        session
            .update_meta(|m| m.name = Some("last-good-name".into()))
            .await
            .unwrap();
        write_extension_outputs(
            &db,
            &BTreeMap::from([(
                "core".into(),
                ExtensionOutput {
                    status: BTreeMap::from([("fixture".into(), "last-good-status".into())]),
                },
            )]),
        )
        .await
        .unwrap();
        let service = ServiceServer::bind(owner.clone(), &socket).await.unwrap();
        let (shutdown, receiver) = watch::channel(());
        let task = tokio::spawn(service.run(receiver));
        let config = EideticaConfig {
            connection: format!("unix://{}", socket.display()),
            login: EideticaLoginConfig {
                username: "failure-fixture".into(),
                password: None,
                passwordless: true,
            },
            sync: None,
        };
        let client = chaz_core::instance::connect_with(&config, ExecutionRole::Client)
            .await
            .unwrap();
        let remote = client.user.open_database(&id).await.unwrap();
        let mut app = App::new(HashSet::new(), tab(&id.to_string(), remote));
        let (tx, mut rx) = mpsc::channel(8);
        for kind in [RefreshKind::Session, RefreshKind::Status] {
            request(&mut app, &id.to_string(), kind, &tx);
        }
        for _ in 0..2 {
            apply(&mut app, rx.recv().await.unwrap(), resolved);
        }
        assert_eq!(app.active().entries.len(), 1);
        assert_eq!(app.active().session_name.as_deref(), Some("last-good-name"));
        assert_eq!(app.active().live.status_segments, ["last-good-status"]);
        println!("CONTROL: service alive -> entries=1 name=last-good-name status_maps=1");
        let dead_db = app.active().session_db.clone();
        drop(shutdown);
        task.await.unwrap().unwrap();
        assert!(
            load_active_turns(&app.active().session_db).await.is_err(),
            "dead backend positive control"
        );
        assert!(
            Session::try_new(
                chaz_core::types::ConversationId(id.to_string()),
                app.active().session_db.clone()
            )
            .await
            .is_err()
        );
        assert!(
            chaz_core::session::try_read_meta_from_db(&app.active().session_db)
                .await
                .is_err()
        );
        assert!(
            chaz_core::extension::try_read_extension_outputs(&app.active().session_db)
                .await
                .is_err()
        );
        for kind in [
            RefreshKind::Session,
            RefreshKind::Status,
            RefreshKind::Activity,
        ] {
            request(&mut app, &id.to_string(), kind, &tx);
        }
        for _ in 0..3 {
            apply(&mut app, rx.recv().await.unwrap(), resolved);
        }
        assert_eq!(app.active().entries[0].content, "last-good-transcript");
        assert_eq!(app.active().session_name.as_deref(), Some("last-good-name"));
        assert_eq!(app.active().live.status_segments, ["last-good-status"]);
        assert!(app.active().live.error.is_some());
        println!("SERVICE LOST: entries=1 name=last-good-name status_maps=1 refresh_error=true");
        let service = ServiceServer::bind(owner, &socket).await.unwrap();
        let (shutdown, receiver) = watch::channel(());
        let task = tokio::spawn(service.run(receiver));
        session.add_entry(message("after-recovery")).await.unwrap();
        // Recovery replaces the complete client generation, not the closed
        // socket, while preserving this view's composer/scroll/incarnation.
        app.input = "draft-during-loss-界".into();
        app.cursor = app.input.len();
        app.active_mut().scroll_offset = 20;
        reconnect::disconnect(&mut app);
        let config = Config {
            execution: Some(ExecutionRole::Client),
            eidetica: Some(config),
            state_dir: Some(dir.path().join("state").display().to_string()),
            ..Default::default()
        };
        let (approval_tx, _approval_rx) = mpsc::channel(8);
        let (notify_tx, _notify_rx) = mpsc::channel(64);
        // The runtime can reopen the DB even while presentation meta is
        // malformed. A hydration failure must not strand its old IPC handle.
        db.with_transaction(|txn| async move {
            txn.get_store::<eidetica::store::DocStore>("meta")
                .await?
                .set_string("agents", "not-json")
                .await
        })
        .await
        .unwrap();
        let mut replacement = reconnect::rebuild(
            None,
            config,
            dir.path().join("config.yaml"),
            vec![(id.to_string(), app.active().live.incarnation, false)],
            approval_tx,
            notify_tx,
        )
        .await
        .map_err(|e| e.0)
        .unwrap();
        let backend = BackendManager::new(&None, replacement.built.secret_store.clone());
        reconnect::apply(&mut app, &mut replacement);
        assert!(
            load_active_turns(&app.active().session_db).await.is_ok(),
            "partial hydration failure retained the dead DB handle"
        );
        assert_eq!(app.active().entries[0].content, "last-good-transcript");
        assert_eq!(app.active().live.status_segments, ["last-good-status"]);
        request(&mut app, &id.to_string(), RefreshKind::Session, &tx);
        apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert_eq!(app.active().entries[0].content, "last-good-transcript");
        assert!(app.active().live.failed(RefreshKind::Session));
        db.with_transaction(|txn| async move {
            txn.get_store::<eidetica::store::DocStore>("meta")
                .await?
                .set_string("agents", "[]")
                .await
        })
        .await
        .unwrap();
        // A different partial failure: inject a failed per-tab open into the
        // rebuilt runtime result. Global health must not hide this tab's need
        // to reopen. The next retry runs the REAL open/observation path.
        app.active_mut().session_db = dead_db;
        app.active_mut().live.unbound = true;
        app.active_mut().live.observation = None;
        replacement.tabs.push(reconnect::Reopened {
            id: id.to_string(),
            incarnation: app.active().live.incarnation,
            result: Err("controlled transient per-tab open failure".into()),
        });
        reconnect::apply(&mut app, &mut replacement);
        assert!(app.backend_available && app.active().live.unbound);
        assert!(app.active().live.rebind_needed);
        app.input = "draft-during-loss-界".into();
        app.cursor = app.input.len();
        assert!(
            input::handle_chat_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.input, "draft-during-loss-界");
        assert!(
            !app.operations.is_pending(),
            "unbound tab admitted a write against dead DB"
        );
        let (binding_tx, mut binding_rx) = mpsc::channel(8);
        let (approval_tx, _approval_rx) = mpsc::channel(8);
        let (notify_tx, _notify_rx) = mpsc::channel(64);
        reconnect::request_tabs(
            &mut app,
            &replacement.built.server,
            &backend,
            &approval_tx,
            &notify_tx,
            &binding_tx,
        );
        reconnect::request_tabs(
            &mut app,
            &replacement.built.server,
            &backend,
            &approval_tx,
            &notify_tx,
            &binding_tx,
        );
        let reconnect::ResultMessage::Tab { ticket, reopened } = binding_rx.recv().await.unwrap()
        else {
            panic!("expected per-tab retry")
        };
        let stale = reconnect::Reopened {
            id: id.to_string(),
            incarnation: app.active().live.incarnation,
            result: Err("stale binding completion".into()),
        };
        assert!(reconnect::apply_retry(&mut app, ticket - 1, stale).is_none());
        assert!(
            app.active().live.rebind_task.is_some(),
            "stale completion dropped replacement binding"
        );
        reconnect::apply_retry(&mut app, ticket, *reopened);
        assert!(!app.active().live.rebind_needed);
        assert!(app.active().live.observation.is_some());
        assert!(
            binding_rx.try_recv().is_err(),
            "duplicate per-tab retry spawned"
        );
        println!(concat!(
            "PARTIAL RECONNECT CONTRACT: PASS; independent handle install, last-good hydration, ",
            "per-tab retry on same runtime, stale-ticket rejection, no dead-handle write admission"
        ));
        for kind in [
            RefreshKind::Session,
            RefreshKind::Status,
            RefreshKind::Activity,
        ] {
            request(&mut app, &id.to_string(), kind, &tx);
        }
        for _ in 0..3 {
            apply(&mut app, rx.recv().await.unwrap(), resolved);
        }
        assert_eq!(app.input, "draft-during-loss-界");
        assert_eq!(app.active().scroll_offset, 20);
        assert_eq!(
            app.active().entries.len(),
            2,
            "existing tab did not recover: {:?}",
            app.active().live.error
        );
        assert!(app.active().live.error.is_none());
        println!(
            "SERVICE RECOVERED: entries=2 name=last-good-name status_maps=1 refresh_error=false"
        );
        println!(
            "REFRESH ERROR CONTRACT: PASS; last-good data retained and existing tab recovered"
        );
        replacement.built.shutdown().await;
        drop(shutdown);
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn local_command_results_survive_reload_in_transcript_order() {
        let (_instance, mut app) = app_with_tabs(&["a"]).await;
        let first = message("first-backend");
        let mut local = message("local-界");
        local.timestamp = first.timestamp + chrono::Duration::seconds(1);
        let mut second = message("second-backend");
        second.timestamp = first.timestamp + chrono::Duration::seconds(2);
        app.active_mut().set_entries(vec![first.clone()]);
        app.active_mut().add_local(local);
        app.active_mut()
            .set_entries(vec![first.clone(), second.clone()]);
        assert_eq!(
            app.active()
                .entries
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>(),
            ["first-backend", "local-界", "second-backend"]
        );
        app.active_mut().set_entries(vec![first, second]);
        assert_eq!(
            app.active().entries.len(),
            3,
            "local result duplicated or discarded"
        );
        app.active_mut().clear_view();
        assert!(app.active().entries.is_empty());
        app.active_mut().set_entries(Vec::new());
        assert!(app.active().entries.is_empty());
    }
}

#[cfg(test)]
mod activity_regressions {
    use super::tests::{resolved, tab};
    use super::*;
    use chaz_core::session::{TurnAttempt, TurnAttemptStatus, TurnRequestId};
    use eidetica::store::Table;

    #[tokio::test]
    async fn unchanged_ordinary_activity_ticks_do_not_read_tables() {
        let (_instance, mut user, reads) =
            super::super::activity_backend::counting_eidetica().await;
        let key = user.get_default_key().unwrap();
        let mut tabs = Vec::new();
        for i in 0..16 {
            let db = user
                .create_database(eidetica::crdt::Doc::new(), &key)
                .await
                .unwrap();
            let tx = db.new_transaction().await.unwrap();
            let attempts = tx
                .get_store::<Table<TurnAttempt>>("turn_attempts")
                .await
                .unwrap();
            for n in 0..1024 {
                let id = format!("old-{n}");
                attempts
                    .set(
                        &id,
                        TurnAttempt {
                            attempt_id: id.clone(),
                            request_id: TurnRequestId::parse(&id),
                            started_at: chrono::Utc::now(),
                            completed_at: Some(chrono::Utc::now()),
                            generation: 1,
                            status: TurnAttemptStatus::Completed,
                        },
                    )
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
            let mut tab = tab(&format!("tab-{i}"), db);
            let before = reads.activity_tables();
            tab.live.activity_evidence = Some(load_active_turns(&tab.session_db).await.unwrap());
            assert!(
                reads.activity_tables() > before,
                "reader must detect positive table control"
            );
            tabs.push(tab);
        }
        for service_mode in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("service.sock");
            let (shutdown, receiver) = tokio::sync::watch::channel(());
            let service_task = if service_mode {
                let service = eidetica::service::ServiceServer::bind(_instance.clone(), &socket)
                    .await
                    .unwrap();
                Some(tokio::spawn(service.run(receiver)))
            } else {
                None
            };
            let client = if service_mode {
                Some(
                    eidetica::Instance::connect(format!("unix://{}", socket.display()))
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            let mut observed = Vec::new();
            for original in &tabs {
                let db = if let Some(client) = &client {
                    client
                        .login_account("test", None)
                        .await
                        .unwrap()
                        .open_database(original.session_db.root_id())
                        .await
                        .unwrap()
                } else {
                    original.session_db.clone()
                };
                let mut tab = tab(&original.session_db_id, db);
                tab.live.activity_evidence =
                    Some(load_active_turns(&tab.session_db).await.unwrap());
                observed.push(tab);
            }
            let mut tabs = observed.into_iter();
            let mut app = App::new(HashSet::new(), tabs.next().unwrap());
            app.tabs.extend(tabs);
            let (tx, mut rx) = mpsc::channel(64);
            let before = (reads.count(), reads.activity_tables());
            tokio::time::pause();
            for _ in 0..10 {
                tokio::time::advance(std::time::Duration::from_secs(5)).await;
                request_tick(&mut app, &tx);
                tokio::task::yield_now().await;
                assert!(
                    !app.tabs.iter().any(|t| t.live.pending()),
                    "unchanged tick launched storage work"
                );
            }
            tokio::time::resume();
            assert!(rx.try_recv().is_err());
            assert_eq!((reads.count(), reads.activity_tables()), before);
            eprintln!(
                "TUI candidate service={service_mode} sessions=16 history=1024 ticks=10 activity_tables=0 raw_entries=0"
            );
            // A failed activity read is retried, and an unrelated successful read
            // must not clear its error or replace its last-good evidence.
            app.active_mut().live.errors[RefreshKind::Activity as usize] =
                Some("read failed".into());
            request_tick(&mut app, &tx);
            apply(&mut app, rx.recv().await.unwrap(), resolved);
            assert!(reads.activity_tables() > before.1);
            assert!(!app.active().live.failed(RefreshKind::Activity));
            drop(app);
            drop(client);
            drop(shutdown);
            if let Some(task) = service_task {
                task.await.unwrap().unwrap();
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn activity_clock_ages_retained_evidence_without_clearing_errors() {
        let (_instance, mut app) = super::tests::app_with_tabs(&["a"]).await;
        let db = app.active().session_db.clone();
        let session = Session::new(chaz_core::types::ConversationId("a".into()), db.clone()).await;
        let mut attempt = session
            .start_turn_attempt(TurnRequestId::parse("future"))
            .await
            .unwrap();
        let now = chrono::Utc::now();
        attempt.started_at = now + chrono::Duration::seconds(3);
        let tx = db.new_transaction().await.unwrap();
        tx.get_store::<Table<TurnAttempt>>("turn_attempts")
            .await
            .unwrap()
            .set(&attempt.attempt_id, attempt.clone())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        app.active_mut().live.activity_evidence = Some(load_active_turns(&db).await.unwrap());
        app.active_mut().live.errors[RefreshKind::Activity as usize] = Some("stale".into());
        sync_error(app.active_mut());
        assert!(!age_activity(&mut app, now));
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        assert!(age_activity(&mut app, now + chrono::Duration::seconds(3)));
        assert_eq!(app.active().active_turns, 1);
        assert!(app.active().live.failed(RefreshKind::Activity));
        tokio::time::advance(std::time::Duration::from_secs(45)).await;
        assert!(age_activity(&mut app, now + chrono::Duration::seconds(48)));
        assert_eq!(app.active().active_turns, 0);
    }
    #[tokio::test]
    async fn failed_transcript_reload_does_not_strand_activity_evidence() {
        let (_instance, mut app) = super::tests::app_with_tabs(&["a"]).await;
        let db = app.active().session_db.clone();
        let mut session =
            Session::new(chaz_core::types::ConversationId("a".into()), db.clone()).await;
        let attempt = session
            .start_turn_attempt(TurnRequestId::parse("active"))
            .await
            .unwrap();
        app.active_mut().live.activity_evidence = Some(load_active_turns(&db).await.unwrap());
        age_activity(&mut app, chrono::Utc::now());
        assert_eq!(app.active().active_turns, 1);
        db.with_transaction(|tx| async move {
            tx.get_store::<eidetica::store::DocStore>("meta")
                .await?
                .set_string("agents", "not-json")
                .await
        })
        .await
        .unwrap();
        session.complete_turn_attempt(&attempt, None).await.unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        request(&mut app, "a", RefreshKind::Session, &tx);
        apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert!(app.active().live.failed(RefreshKind::Session));
        request_tick(&mut app, &tx);
        for _ in 0..2 {
            apply(&mut app, rx.recv().await.unwrap(), resolved);
        }
        assert_eq!(
            app.active().active_turns,
            0,
            "failed transcript hid completion"
        );
        assert!(app.active().live.failed(RefreshKind::Session));
        assert!(!app.active().live.failed(RefreshKind::Activity));
    }
}
