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
#[derive(Debug, Default)]
pub(super) struct TabLive {
    /// Full session reload (entries + meta + activity) after a write.
    pub(super) reload: RefreshSlot,
    /// Extension status strip read.
    pub(super) status: RefreshSlot,
    /// Live-turn claim count, polled on the activity tick.
    pub(super) activity: RefreshSlot,
    /// Ticket of the newest read whose `active_turns` has been applied, so a
    /// slow tick can't overwrite a newer reload's count (or vice versa).
    activity_applied: u64,
    /// Last extension status segments read for this session. Shown as-is
    /// while a refresh is pending; never borrowed from another tab.
    pub(super) status_segments: Vec<String>,
    /// Most recent backend read failure for this tab, cleared by the next
    /// successful read. Shown in the status bar so a failed refresh doesn't
    /// pass for fresh data.
    pub(super) error: Option<String>,
    /// Bumped whenever `entries` or `expanded_entries` change, so the
    /// transcript render cache knows when its wrapped lines are stale.
    pub(super) transcript_rev: u64,
    /// Wrapped transcript reused across frames until `transcript_rev`, the
    /// width or a display toggle changes.
    pub(super) render: view::TranscriptCache,
}

/// What a session reload read from storage.
pub(super) struct SessionSnapshot {
    pub(super) entries: Vec<SessionEntry>,
    pub(super) meta: SessionMeta,
    pub(super) active_turns: Result<usize, String>,
}

/// A completed background read, routed back to the run loop.
pub(super) enum Refresh {
    Session {
        session_db_id: String,
        ticket: u64,
        snapshot: Box<SessionSnapshot>,
    },
    Status {
        session_db_id: String,
        ticket: u64,
        segments: Vec<String>,
    },
    Activity {
        session_db_id: String,
        ticket: u64,
        active_turns: Result<usize, String>,
    },
}

/// The kinds of read a tab can request; returned as follow-ups so the run
/// loop (not the apply step) decides how to spawn them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RefreshKind {
    Session,
    Status,
    Activity,
}

/// Result of applying one [`Refresh`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Applied {
    /// Visible state changed; the next frame must be drawn.
    pub(super) redraw: bool,
    /// A request arrived while this read ran; issue one more.
    pub(super) follow_up: Option<(String, RefreshKind)>,
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

    pub(super) fn set_entries(&mut self, entries: Vec<SessionEntry>) {
        self.entries = entries;
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
    load: impl FnOnce(u64) -> Fut,
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
    };
    if !slot.begin(ticket) {
        return;
    }
    let fut = load(ticket);
    let tx = tx.clone();
    // The slot owns the task so closing its tab or exiting the TUI cancels
    // the read, including one stalled forever on the backend.
    slot.task = Some(tokio::spawn(async move {
        let _ = tx.send(fut.await).await;
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
    let db = app.tabs[idx].session_db.clone();
    let id = session_db_id.to_string();
    request_with(app, session_db_id, kind, tx, move |ticket| async move {
        match kind {
            RefreshKind::Session => Refresh::Session {
                snapshot: Box::new(load_session_snapshot(&id, db).await),
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

async fn load_session_snapshot(session_db_id: &str, db: eidetica::Database) -> SessionSnapshot {
    let active_turns = load_active_turns(&db).await;
    let session = Session::new(
        chaz_core::types::ConversationId(session_db_id.to_string()),
        db,
    )
    .await;
    SessionSnapshot {
        entries: session.entries().to_vec(),
        meta: session.read_meta().await,
        active_turns,
    }
}

pub(super) async fn load_active_turns(db: &eidetica::Database) -> Result<usize, String> {
    Session::active_turn_attempts(db)
        .await
        .map(|claims| claims.len())
        .map_err(|e| e.to_string())
}

/// Flatten every extension's status map from the session's
/// `extension_outputs` store (written by the daemon at the turn boundary)
/// into render-ready segments, ordered by extension name then key.
pub(super) async fn load_status_segments(db: &eidetica::Database) -> Vec<String> {
    chaz_core::extension::read_extension_outputs(db)
        .await
        .into_values()
        .flat_map(|o| o.status.into_values())
        .collect()
}

fn apply_active_turns(tab: &mut Tab, ticket: u64, active_turns: Result<usize, String>) -> bool {
    if ticket < tab.live.activity_applied {
        return false;
    }
    tab.live.activity_applied = ticket;
    match active_turns {
        Ok(n) => {
            let changed = tab.active_turns != n || tab.live.error.is_some();
            tab.active_turns = n;
            tab.live.error = None;
            changed
        }
        Err(e) => {
            tracing::warn!(session = %tab.session_db_id, "Live turn refresh failed: {e}");
            tab.live.error = Some(e);
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
    };
    let Some(again) = slot.finish(ticket) else {
        return Applied::default();
    };
    let follow_up = again.then(|| (session_db_id.clone(), kind));

    let redraw = match msg {
        Refresh::Status { segments, .. } => {
            let tab = &mut app.tabs[idx];
            let changed = tab.live.status_segments != segments;
            tab.live.status_segments = segments;
            changed && is_active
        }
        Refresh::Activity { active_turns, .. } => {
            apply_active_turns(&mut app.tabs[idx], ticket, active_turns) && is_active
        }
        Refresh::Session { snapshot, .. } => {
            apply_session_snapshot(app, idx, ticket, *snapshot, resolve);
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

fn apply_session_snapshot(
    app: &mut App,
    idx: usize,
    ticket: u64,
    snapshot: SessionSnapshot,
    resolve: impl FnOnce(&str, &SessionMeta) -> Resolved,
) {
    let SessionSnapshot {
        entries,
        meta,
        active_turns,
    } = snapshot;
    let Resolved {
        effective_model,
        context_budget,
        roster,
    } = resolve(&app.tabs[idx].current_agent, &meta);

    let tab = &mut app.tabs[idx];
    tab.set_entries(entries);
    tab.session_name = meta.name.clone();
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
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};
    use ratatui::backend::TestBackend;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// App with one tab per id. Every tab shares one live in-memory DB:
    /// loaders are injected, so the handle is never read.
    async fn app_with_tabs(ids: &[&str]) -> (Instance, App) {
        let (instance, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewUser::passwordless("tab-refresh"),
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

    fn tab(id: &str, db: eidetica::Database) -> Tab {
        Tab {
            session_db_id: id.to_string(),
            session_db: db,
            entries: Vec::new(),
            scroll_offset: 0,
            pending_approval: None,
            active_turns: 0,
            current_agent: "chaz".into(),
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

    fn resolved(_: &str, _: &SessionMeta) -> Resolved {
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
            segments: vec![segment.into()],
        }
    }

    fn snapshot(id: &str, ticket: u64, entries: Vec<SessionEntry>, turns: usize) -> Refresh {
        Refresh::Session {
            session_db_id: id.into(),
            ticket,
            snapshot: Box::new(SessionSnapshot {
                entries,
                meta: SessionMeta {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
                active_turns: Ok(turns),
            }),
        }
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
        let release_reload = request_gated(&mut app, "a", RefreshKind::Session, &tx, |t| {
            snapshot("a", t, vec![message("reloaded")], 1)
        });
        assert!(app.active().live.status.is_pending());
        assert!(app.active().live.reload.is_pending());

        let frame = tokio::time::timeout(Duration::from_secs(1), async {
            for c in "hello".chars() {
                input::handle_chat_key(&mut app, key(KeyCode::Char(c))).await;
            }
            input::handle_chat_key(&mut app, key(KeyCode::PageUp)).await;
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
                    snapshot("a", t, vec![message("first")], 0)
                }
            });
        }
        assert_eq!(spawned.load(Ordering::SeqCst), 1);
        releases.remove(0).send(()).unwrap();
        let applied = apply(&mut app, rx.recv().await.unwrap(), resolved);
        assert_eq!(
            applied.follow_up,
            Some(("a".to_string(), RefreshKind::Session))
        );

        let release = request_gated(&mut app, "a", RefreshKind::Session, &tx, |t| {
            snapshot("a", t, vec![message("first"), message("second")], 0)
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
            snapshot("b", t, vec![message("ghost")], 0)
        });
        release.send(()).unwrap();
        // A result already queued before close still needs the identity guard.
        let late = rx.recv().await.unwrap();
        close_tab_at(&mut app, 1);
        assert_eq!(apply(&mut app, late, resolved), Applied::default());
        assert!(app.tabs[0].entries.is_empty());

        // Closed then reopened: the new tab never asked for the old read.
        let release = request_gated(&mut app, "a", RefreshKind::Session, &tx, |t| {
            snapshot("a", t, vec![message("old tab")], 0)
        });
        release.send(()).unwrap();
        let late = rx.recv().await.unwrap();
        let db = app.tabs[0].session_db.clone();
        app.tabs.push(tab("z", db.clone()));
        close_tab_at(&mut app, 0);
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
            snapshot("a", t, Vec::new(), 0)
        });
        let exiting = request_gated(&mut app, "b", RefreshKind::Status, &tx, |t| {
            status("b", t, "unused")
        });
        tokio::task::yield_now().await;
        assert!(!closed.is_closed() && !exiting.is_closed());

        close_tab_at(&mut app, 0);
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
        let poll = request_gated(&mut app, "a", RefreshKind::Activity, &tx, |t| {
            Refresh::Activity {
                session_db_id: "a".into(),
                ticket: t,
                active_turns: Ok(3),
            }
        });
        let reload = request_gated(&mut app, "a", RefreshKind::Session, &tx, |t| {
            snapshot("a", t, Vec::new(), 0)
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
                active_turns: Ok(0),
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
}
