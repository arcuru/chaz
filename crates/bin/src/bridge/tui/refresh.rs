//! Pinned session hydration and in-memory application of the session projection.

use super::*;
use chaz_core::session::TurnActivity;

/// Backend-derived per-tab state plus its refresh bookkeeping. Grouped so a
/// `Tab` literal only needs `live: TabLive::default()`.
#[derive(Default)]
pub(super) struct TabLive {
    /// Inspection-only Worker: no composer, steering, settings or runtime registration.
    pub(super) worker_observer: bool,
    pub(super) workers: workers::View,
    pub(super) jump_call: Option<tool_activity::CallKey>,
    pub(super) covered: Option<eidetica::Snapshot>,
    pub(super) entry_ids: Vec<Option<String>>,
    pub(super) tool_activity: tool_activity::ToolActivity,
    pub(super) expanded_runs: HashSet<tool_activity::RunKey>,
    pub(super) expanded_tools: HashSet<tool_activity::CallKey>,
    pub(super) steering: Option<chaz_core::session::steering::SteeringView>,
    pub(super) input_attempt: Option<String>,
    pub(super) inputs: Vec<chaz_core::session::steering::JobInput>,
    pub(super) input_feedback: String,
    pub(super) input_retry:
        Arc<tokio::sync::Mutex<Option<chaz_core::session::steering::JobInputRequest>>>,
    pub(super) activity_evidence: Option<TurnActivity>,
    /// Last extension status segments read for this session. Shown as-is
    /// while a refresh is pending; never borrowed from another tab.
    pub(super) status_segments: Vec<String>,
    /// An unresolved binding or per-kind read failure. A successful unrelated
    /// read cannot clear it or make cached data pass for fresh data.
    pub(super) error: Option<String>,
    errors: [Option<String>; 4],
    pub(super) meta: SessionMeta,
    pub(super) writer_keys: Vec<eidetica::auth::crypto::PublicKey>,
    pub(super) agent_config_stale: bool,
    pub(super) telemetry: Option<chaz_core::session::usage::SessionTelemetry>,
    /// View-local command/error output is not stored in the session DB.
    local_entries: Vec<SessionEntry>,
    pub(super) incarnation: u64,
    pub(super) observation: Option<Observation>,
    pub(super) initial_handled: Option<tokio::sync::oneshot::Sender<()>>,
    pub(super) snapshot_started: Option<tokio::time::Instant>,
    pub(super) snapshot_overdue: bool,
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
        self.workers.cancel();
        self.covered = None;
        self.snapshot_started = None;
        self.snapshot_overdue = false;
        self.initial_handled = None;
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
    pub(super) fn pending(&self) -> bool {
        self.snapshot_started.is_some() || self.rebind_task.is_some()
    }
}

/// What a session reload read from storage.
pub(super) struct SessionSnapshot {
    pub(super) tips: eidetica::Snapshot,
    pub(super) steering: Option<chaz_core::session::steering::SteeringView>,
    pub(super) input_attempt: Option<String>,
    pub(super) inputs: Vec<chaz_core::session::steering::JobInput>,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) entry_ids: Vec<String>,
    pub(super) tool_activity: tool_activity::ToolActivity,
    pub(super) telemetry: chaz_core::session::usage::SessionTelemetry,
    pub(super) meta: SessionMeta,
    pub(super) writer_keys: Vec<eidetica::auth::crypto::PublicKey>,
    pub(super) active_turns: Result<TurnActivity, String>,
    pub(super) status: Result<Vec<String>, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RefreshKind {
    Session,
    Status,
    Activity,
    Job,
}

pub(super) fn age_activity(app: &mut App, now: chrono::DateTime<chrono::Utc>) -> bool {
    let mut redraw = false;
    for tab in &mut app.tabs {
        if tab.live.unbound {
            continue;
        }
        if let Some(evidence) = &tab.live.activity_evidence
            && tab.live.tool_activity.age(evidence, now)
        {
            tab.live.transcript_rev = tab.live.transcript_rev.wrapping_add(1);
            redraw = true;
        }
        if tab.job.is_none()
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

impl Tab {
    /// Record a change to the transcript source (`entries` /
    /// `expanded_entries`) for the render cache.
    pub(super) fn touch_transcript(&mut self) {
        self.live.transcript_rev = self.live.transcript_rev.wrapping_add(1);
    }

    #[cfg(test)]
    pub(super) fn set_entries(&mut self, entries: Vec<SessionEntry>) {
        self.set_identified_entries(entries, Vec::new());
    }

    fn set_identified_entries(&mut self, entries: Vec<SessionEntry>, ids: Vec<String>) {
        let mut ids = ids.into_iter();
        let mut rows: Vec<_> = entries.into_iter().map(|e| (ids.next(), e)).collect();
        rows.extend(self.live.local_entries.iter().cloned().map(|e| (None, e)));
        // Sort the identity with its row; local output never acquires provenance.
        rows.sort_by_key(|(_, entry)| entry.timestamp);
        (self.live.entry_ids, self.entries) = rows.into_iter().unzip();
        self.live.telemetry = None;
        self.touch_transcript();
    }

    pub(super) fn add_local(&mut self, entry: SessionEntry) {
        self.live.local_entries.push(entry.clone());
        self.entries.push(entry);
        self.live.entry_ids.push(None);
        self.touch_transcript();
    }

    pub(super) fn clear_view(&mut self) {
        self.entries.clear();
        self.live.entry_ids.clear();
        self.live.tool_activity = Default::default();
        self.live.expanded_runs.clear();
        self.live.expanded_tools.clear();
        self.live.telemetry = None;
        self.live.local_entries.clear();
        self.scroll_offset = 0;
        self.expanded_entries.clear();
        self.touch_transcript();
    }
}

#[cfg(test)]
pub(super) async fn load_session_snapshot(
    _session_db_id: &str,
    db: eidetica::Database,
) -> Result<SessionSnapshot, String> {
    let tips = db.snapshot().await.map_err(|e| e.to_string())?;
    load_session_snapshot_at(&db, tips).await
}

pub(super) async fn load_session_snapshot_at(
    db: &eidetica::Database,
    tips: eidetica::Snapshot,
) -> Result<SessionSnapshot, String> {
    async {
        let session = Session::try_new_at(
            chaz_core::types::ConversationId(db.root_id().to_string()),
            db.clone(),
            &tips,
        )
        .await?;
        snapshot_from_session(&session, db, tips).await
    }
    .await
    .map_err(|e| e.to_string())
}

pub(super) async fn snapshot_from_session(
    session: &Session,
    db: &eidetica::Database,
    tips: eidetica::Snapshot,
) -> anyhow::Result<SessionSnapshot> {
    let entries = session.entries().to_vec();
    let entry_ids = session
        .entries_with_ids()
        .map(|(id, _)| id.as_str().to_owned())
        .collect();
    let tool_activity = tool_activity::ToolActivity::load(
        db,
        &tips,
        session
            .entries_with_ids()
            .map(|(id, entry)| (id.as_str(), entry)),
    )
    .await?;
    let telemetry = chaz_core::session::usage::telemetry_at_snapshot(db, &tips, &entries).await?;
    let meta = chaz_core::session::try_read_meta_at(db, &tips).await?;
    let steering = Session::steering_view_at(db, &tips).await?;
    let writer_keys = chaz_core::session::SessionRegistry::direct_writer_keys_at(db, &tips).await?;
    let status = load_status_at(db, &tips).await;
    let now = chrono::Utc::now();
    let attempts = steering.attempts_at(now);
    Ok::<_, anyhow::Error>(SessionSnapshot {
        input_attempt: match attempts.as_slice() {
            [id] => Some(id.clone()),
            _ => None,
        },
        inputs: steering.inputs_at(now),
        active_turns: Ok(steering.activity().clone()),
        status,
        steering: Some(steering),
        tips,
        entries,
        entry_ids,
        tool_activity,
        telemetry,
        meta,
        writer_keys,
    })
}

/// Expiry has no write. Age only disposable display evidence; actions still
/// check native current authority and current attempt state independently.
pub(super) fn age_steering(app: &mut App, now: chrono::DateTime<chrono::Utc>) -> bool {
    let mut redraw = false;
    for tab in &mut app.tabs {
        if tab.live.unbound {
            continue;
        }
        if let Some(view) = &tab.live.steering {
            let attempts = view.attempts_at(now);
            let attempt = if let Some(job) = &tab.job {
                jobs::attempt_id(job.state.as_ref()).map(str::to_owned)
            } else {
                match attempts.as_slice() {
                    [id] => Some(id.clone()),
                    _ => None,
                }
            };
            let inputs = view.inputs_at(now);
            redraw |= tab.live.input_attempt != attempt || tab.live.inputs != inputs;
            tab.live.input_attempt = attempt;
            tab.live.inputs = inputs;
            if let Some(job) = &mut tab.job
                && let Some(chaz_core::session::jobs::JobState::StartedUnknown {
                    attempt_id,
                    activity_recent,
                }) = &mut job.state
            {
                let recent = view.active_at(now).any(|id| id == attempt_id);
                redraw |= *activity_recent != recent;
                *activity_recent = recent;
                tab.active_turns = usize::from(*activity_recent);
            }
        }
    }
    redraw
}

#[cfg(test)]
pub(super) async fn load_active_turns(db: &eidetica::Database) -> Result<TurnActivity, String> {
    Session::turn_activity(db).await.map_err(|e| e.to_string())
}

pub(super) async fn load_status_at(
    db: &eidetica::Database,
    tips: &eidetica::Snapshot,
) -> Result<Vec<String>, String> {
    chaz_core::extension::try_read_extension_outputs_at(db, tips)
        .await
        .map(status_segments)
        .map_err(|e| e.to_string())
}

fn status_segments(
    outputs: std::collections::BTreeMap<String, chaz_core::extension::ExtensionOutput>,
) -> Vec<String> {
    outputs
        .into_values()
        .flat_map(|output| output.status.into_values())
        .collect()
}

pub(super) fn apply_watched(
    app: &mut App,
    idx: usize,
    snapshot: Result<presentation::TabSnapshot, String>,
) -> bool {
    let is_job = app.tabs[idx].job.is_some();
    let kind = if is_job {
        RefreshKind::Job
    } else {
        RefreshKind::Session
    };
    record_error(&mut app.tabs[idx], kind, snapshot.as_ref().err().cloned());
    match snapshot {
        Ok((snapshot, state)) => {
            apply_session_snapshot(app, idx, snapshot);
            if let Some(state) = state {
                jobs::apply_state(&mut app.tabs[idx], state);
            }
            return true;
        }
        Err(error) if is_job => jobs::apply_error(&mut app.tabs[idx], error),
        Err(_) => app.tabs[idx].live.covered = None,
    }
    false
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

fn apply_active_turns(tab: &mut Tab, active_turns: Result<TurnActivity, String>) {
    match active_turns {
        Ok(evidence) => {
            let n = evidence.active_at(chrono::Utc::now()).count();
            let tool_changed = tab.live.tool_activity.age(&evidence, chrono::Utc::now());
            if tool_changed {
                tab.touch_transcript();
            }
            tab.live.activity_evidence = Some(evidence);
            let approval_closed = tab
                .pending_approval
                .as_ref()
                .is_some_and(|ex| ex.decision_tx.is_closed());
            if approval_closed {
                tab.pending_approval = None;
            }
            tab.active_turns = n;
            record_error(tab, RefreshKind::Activity, None);
        }
        Err(e) => {
            tracing::warn!(session = %tab.session_db_id, "Live turn refresh failed: {e}");
            record_error(tab, RefreshKind::Activity, Some(e));
        }
    }
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
#[cfg(test)]
pub(super) fn resolve(
    server: &Server,
    backend: &BackendManager,
    current_agent: &str,
    meta: &SessionMeta,
) -> Resolved {
    resolve_from(
        server,
        backend,
        current_agent,
        meta,
        &ModelMetadata::default(),
        &HashMap::new(),
        server
            .agent_index()
            .find_by_name(current_agent)
            .as_ref()
            .map(|entry| &entry.db_id),
    )
}

pub(super) fn resolve_from(
    server: &Server,
    backend: &BackendManager,
    current_agent: &str,
    meta: &SessionMeta,
    observed: &ModelMetadata,
    defaults: &HashMap<eidetica::entry::ID, components::Defaults>,
    agent_id: Option<&eidetica::entry::ID>,
) -> Resolved {
    let (default_model, agent_cap) = agent_defaults(server, defaults, current_agent, agent_id);
    let session_model = meta.resolve_model_for_agent(current_agent);
    Resolved {
        effective_model: backend.resolve_model_name(session_model.or(default_model.as_deref())),
        context_budget: server
            .observed_context_budget(session_model, default_model.as_deref(), agent_cap, observed)
            .tokens,
        roster: build_roster(server, backend, meta, defaults),
    }
}

pub(super) fn agent_defaults(
    server: &Server,
    defaults: &HashMap<eidetica::entry::ID, components::Defaults>,
    name: &str,
    id: Option<&eidetica::entry::ID>,
) -> (Option<String>, Option<usize>) {
    if let Some(id) = id {
        return defaults
            .get(id)
            .map(|cfg| (cfg.model.clone(), cfg.cap))
            .unwrap_or_default();
    }
    #[cfg(test)]
    if let Some(agent) = server.agents().get(name) {
        return (agent.default_model, agent.max_context_tokens);
    }
    let _ = (server, name);
    (None, None)
}

pub(super) fn apply_model_metadata(app: &mut App, server: &Server, state: ModelMetadata) {
    // The replacement generation may still be starting. Keep last-good
    // numbers until there is a real observed (possibly empty) component.
    if state.ready {
        app.model_metadata = state;
    } else {
        app.model_metadata.error = state.error;
    }
    derive_models(app, server);
}

pub(super) fn derive_models(app: &mut App, server: &Server) {
    for idx in 0..app.tabs.len() {
        derive_tab(app, server, idx);
    }
}

pub(super) fn apply_initial_models(app: &mut App, server: &Server) {
    for idx in 0..app.tabs.len() {
        if let Some(handled) = app.tabs[idx].live.initial_handled.take() {
            derive_tab(app, server, idx);
            let _ = handled.send(());
        }
    }
}

pub(super) fn derive_tab(app: &mut App, server: &Server, idx: usize) {
    let tab = &mut app.tabs[idx];
    if tab.live.unbound {
        return;
    }
    let (name, entry) = components::select_agent(server, &tab.live.meta, &tab.live.writer_keys);
    let agent_id = entry.as_ref().map(|entry| &entry.db_id);
    tab.live.agent_config_stale =
        agent_id.is_some_and(|id| app.agent_config_errors.contains_key(id));
    let Resolved {
        effective_model,
        context_budget,
        roster,
    } = resolve_from(
        server,
        server.backend(),
        &name,
        &tab.live.meta,
        &app.model_metadata,
        &app.agent_defaults,
        agent_id,
    );
    tab.model_pin = tab
        .live
        .meta
        .resolve_model_for_agent(&name)
        .map(str::to_owned);
    tab.current_agent = name;
    tab.effective_model = effective_model;
    tab.context_budget = context_budget;
    tab.roster = roster;
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

/// A failed transcript read doesn't hide independently readable turn evidence
/// at the same frontier. The enclosing tab still displays the transcript error.
pub(super) fn replace_steering(
    app: &mut App,
    idx: usize,
    steering: Result<chaz_core::session::steering::SteeringView, String>,
) {
    let tab = &mut app.tabs[idx];
    let turns = steering
        .as_ref()
        .map(|view| view.activity().clone())
        .map_err(Clone::clone);
    if let Ok(steering) = steering {
        tab.live.steering = Some(steering);
    }
    apply_active_turns(tab, turns);
    age_steering(app, chrono::Utc::now());
}

fn apply_session_snapshot(app: &mut App, idx: usize, snapshot: SessionSnapshot) {
    let SessionSnapshot {
        tips,
        steering,
        input_attempt,
        inputs,
        entries,
        entry_ids,
        tool_activity,
        telemetry,
        meta,
        writer_keys,
        active_turns,
        status,
    } = snapshot;
    let tab = &mut app.tabs[idx];
    tab.live.covered = (active_turns.is_ok() && status.is_ok()).then_some(tips);
    replace_status(tab, status);
    tab.live.steering = steering;
    tab.live.input_attempt = input_attempt;
    tab.live.inputs = inputs;
    tab.set_identified_entries(entries, entry_ids);
    tab.live.tool_activity = tool_activity;
    tab.live.telemetry = Some(telemetry);
    tab.session_name = meta.name.clone();
    tab.model_pin = meta
        .resolve_model_for_agent(&tab.current_agent)
        .map(str::to_owned);
    tab.live.meta = meta.clone();
    tab.live.writer_keys = writer_keys;
    apply_active_turns(tab, active_turns);
    if let Some(activity) = &tab.live.activity_evidence {
        tab.live.tool_activity.age(activity, chrono::Utc::now());
    }
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

    project_open_row(app, idx);
}

#[cfg(test)]
pub(super) fn derive_fixture(tab: &mut Tab, resolve: impl FnOnce(&str, &SessionMeta) -> Resolved) {
    let model = resolve(&tab.current_agent, &tab.live.meta);
    tab.effective_model = model.effective_model;
    tab.context_budget = model.context_budget;
    tab.roster = model.roster;
}

/// Test renderers can load one real pinned view without wiring a subscriber.
#[cfg(test)]
pub(super) async fn fixture_reload(
    app: &mut App,
    resolve: impl FnOnce(&str, &SessionMeta) -> Resolved,
) {
    let idx = app.active_tab;
    let db = app.tabs[idx].session_db.clone();
    let result = async {
        let tips = db.snapshot().await.map_err(|e| e.to_string())?;
        if app.tabs[idx].job.is_some() {
            let loaded = jobs::snapshot_at(&db, tips).await?;
            Ok((loaded.session, Some(loaded.state)))
        } else {
            Ok((load_session_snapshot_at(&db, tips).await?, None))
        }
    }
    .await;
    if apply_watched(app, idx, result) {
        derive_fixture(&mut app.tabs[idx], resolve);
    }
}

#[cfg(test)]
#[path = "hydration_tests.rs"]
mod hydration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewAccount};
    use ratatui::backend::TestBackend;

    /// App with one tab per id. Every tab shares one live in-memory DB:
    /// loaders are injected, so the handle is never read.
    pub(super) async fn app_with_tabs(ids: &[&str]) -> (Instance, App) {
        let (instance, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::with_password("tab-refresh", "test-password"),
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
            expand_all: false,
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

    #[tokio::test]
    async fn per_call_telemetry_refresh_renders_alias_missing_usage_and_multi_agent_totals() {
        use chaz_core::runtime::{ResponseMetadata, TokenUsage};
        use chaz_core::session::{
            TurnCallIdentity, TurnRequestId, TurnTranscriptMessage, TurnTranscriptRecord,
        };
        let (_instance, mut app) = app_with_tabs(&["a"]).await;
        let db = app.active().session_db.clone();
        let session = Session::new(chaz_core::types::ConversationId("a".into()), db).await;
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
            fixture_reload(&mut app, |_, _| Resolved {
                effective_model: "gpt-sol-latest".into(),
                context_budget: 10_000,
                roster: Vec::new(),
            })
            .await;
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
                apply_watched(&mut app, 0, Err("controlled telemetry read failure".into()));
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

    fn draw_chat(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        let segments = app.active().live.status_segments.clone();
        terminal.draw(|f| view::ui_chat(f, app, &segments)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    /// A burst of session writes while a reload runs spawns no extra reads:
    /// it collapses into exactly one follow-up.
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
            NewAccount::with_password("failure-fixture", "test-password"),
        )
        .await
        .unwrap();
        // The owner initializes the login's roots before any client starts.
        chaz_core::session::roots::open(
            &mut user,
            chaz_core::session::roots::RootSetup::Initialize,
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
                password: Some("test-password".into()),
            },
            sync: None,
        };
        let client = chaz_core::instance::connect_with(&config, ExecutionRole::Client)
            .await
            .unwrap();
        let remote = client.user.open_database(&id).await.unwrap();
        let mut app = App::new(HashSet::new(), tab(&id.to_string(), remote));
        fixture_reload(&mut app, resolved).await;
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
        fixture_reload(&mut app, resolved).await;
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
        reconnect::disconnect(&mut app, "Backend disconnected");
        let config = Config {
            execution: Some(ExecutionRole::Client),
            eidetica: Some(config),
            state_dir: Some(dir.path().join("state").display().to_string()),
            ..Default::default()
        };
        let (approval_tx, _approval_rx) = mpsc::channel(8);
        let (notify_tx, _notify_rx) = presentation::channel();
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
        let built = reconnect::rebuild(None, config, dir.path().join("config.yaml"))
            .await
            .map_err(|e| e.0)
            .unwrap();
        let backend = BackendManager::new(&None, built.secret_store.clone());
        let (binding_tx, mut binding_rx) = mpsc::channel(8);
        reconnect::connected(&mut app);
        reconnect::request_tabs(
            &mut app,
            &built.server,
            &backend,
            &approval_tx,
            &notify_tx,
            &binding_tx,
        );
        let reconnect::ResultMessage::Tab { ticket, reopened } = binding_rx.recv().await.unwrap()
        else {
            panic!("expected active tab binding");
        };
        reconnect::apply_retry(&mut app, ticket, *reopened);
        assert!(
            load_active_turns(&app.active().session_db).await.is_ok(),
            "partial hydration failure retained the dead DB handle"
        );
        assert_eq!(app.active().entries[0].content, "last-good-transcript");
        assert_eq!(app.active().live.status_segments, ["last-good-status"]);
        let mut reads = presentation::Reads::default();
        reads.start_initial(&mut app);
        presentation::apply(
            &mut app,
            reads.join_next().await.unwrap().unwrap(),
            resolved,
        );
        assert_eq!(app.active().entries[0].content, "last-good-transcript");
        assert!(app.active().live.error.is_some());
        db.with_transaction(|txn| async move {
            txn.get_store::<eidetica::store::DocStore>("meta")
                .await?
                .set_string("agents", "[]")
                .await
        })
        .await
        .unwrap();
        // Global health must not hide a tab still waiting for a fresh handle.
        // The next retry runs the real open/observation path.
        app.active_mut().session_db = dead_db;
        app.active_mut().live.unbound = true;
        app.active_mut().live.observation = None;
        app.active_mut().live.rebind_needed = true;
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
        reconnect::request_tabs(
            &mut app,
            &built.server,
            &backend,
            &approval_tx,
            &notify_tx,
            &binding_tx,
        );
        reconnect::request_tabs(
            &mut app,
            &built.server,
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
        reads.start_initial(&mut app);
        presentation::apply(
            &mut app,
            reads.join_next().await.unwrap().unwrap(),
            resolved,
        );
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
        built.shutdown().await;
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
    use super::*;
    use chaz_core::session::{TurnAttempt, TurnRequestId};
    use eidetica::store::Table;

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
        assert!(app.active().live.error.is_some());
        tokio::time::advance(std::time::Duration::from_secs(45)).await;
        assert!(age_activity(&mut app, now + chrono::Duration::seconds(48)));
        assert_eq!(app.active().active_turns, 0);
    }
}
