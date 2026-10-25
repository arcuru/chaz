//! Ordered accepted actions, separate from cancellable presentation reads.
//! One owned worker and a bounded FIFO; never retry a possibly committed write.
use super::*;
use std::{collections::VecDeque, pin::Pin, time::Duration};

const CAPACITY: usize = 32;
const WRITE_WARNING: Duration = Duration::from_secs(10);
type Work = Pin<Box<dyn Future<Output = Result<Effect, String>> + Send>>;

#[derive(Clone)]
pub(super) struct Origin {
    tab: Option<(String, u64)>,
    navigation: u64,
    mode: TuiMode,
}
impl Origin {
    pub(super) fn capture(app: &App) -> Self {
        Self {
            tab: app
                .current()
                .map(|t| (t.session_db_id.clone(), t.live.incarnation)),
            navigation: app.navigation_generation,
            mode: app.mode,
        }
    }
    fn tab_index(&self, app: &App) -> Option<usize> {
        let (id, incarnation) = self.tab.as_ref()?;
        app.tab_index_for(id)
            .filter(|&i| app.tabs[i].live.incarnation == *incarnation)
    }
    fn navigation_valid(&self, app: &App) -> bool {
        self.navigation == app.navigation_generation
            && self.mode == app.mode
            && self
                .tab
                .as_ref()
                .is_none_or(|_| self.tab_index(app).is_some())
    }
}

pub(super) enum Effect {
    /// A non-session mutation can affect every tab's model/agent display.
    Global(Box<Effect>),
    Outcome(CommandOutcome),
    Opened(Box<Tab>),
    Open(Box<Open>),
    Sent,
    JobSent(String),
    Renamed {
        id: String,
        name: Option<String>,
    },
    /// A description command committed; patch the cached picker row.
    Described {
        id: String,
        description: Option<String>,
        text: String,
    },
    Settings(String),
    Diff(AgentDiffView),
}
struct Pending {
    ticket: u64,
    origin: Origin,
    write: bool,
    /// Original accepted draft retained until its outcome is known.
    input: Option<String>,
    work: Option<Work>,
}
pub(super) struct Completion {
    pending: Pending,
    result: Result<Effect, String>,
}
pub(super) struct Operations {
    queue: VecDeque<Pending>,
    task: Option<tokio::task::JoinHandle<()>>,
    tx: mpsc::Sender<Completion>,
    rx: mpsc::Receiver<Completion>,
    started: Option<tokio::time::Instant>,
    write: bool,
    warned: bool,
    pub(super) notice: Option<String>,
    exit_confirm: bool,
    current_input: Option<String>,
    uncertain_inputs: Vec<String>,
    uncertainty: Vec<String>,
    next_ticket: u64,
    current_ticket: Option<u64>,
    current_origin: Option<Origin>,
    creation_ticket: Option<u64>,
}
impl Default for Operations {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(1);
        Self {
            queue: VecDeque::new(),
            task: None,
            tx,
            rx,
            started: None,
            write: false,
            warned: false,
            notice: None,
            exit_confirm: false,
            current_input: None,
            uncertain_inputs: Vec::new(),
            uncertainty: Vec::new(),
            next_ticket: 0,
            current_ticket: None,
            current_origin: None,
            creation_ticket: None,
        }
    }
}
impl Drop for Operations {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
impl Operations {
    pub(super) fn indicator(&self) -> String {
        let mut status = String::new();
        if let Some(notice) = &self.notice {
            status.push_str(&format!(" | ⚠ {notice}"));
        }
        if self.is_pending() {
            status.push_str(&format!(
                " | {} action(s) pending",
                self.queue.len() + usize::from(self.task.is_some())
            ));
        }
        if self.warned {
            status.push_str(concat!(
                " | ⚠ Write still pending; outcome unknown. ",
                "Not cancelled or retried; later actions remain queued"
            ));
        }
        if let Some(uncertain) = self.uncertainty.last() {
            status.push_str(&format!(" | ⚠ {uncertain}"));
        }
        status
    }
    pub(super) fn has_capacity(&self) -> bool {
        self.queue.len() + usize::from(self.task.is_some()) < CAPACITY
    }
    pub(super) fn is_creating_session(&self) -> bool {
        self.creation_ticket.is_some()
    }
    pub(super) fn is_pending(&self) -> bool {
        self.task.is_some() || !self.queue.is_empty()
    }
    pub(super) fn submit(
        &mut self,
        origin: Origin,
        write: bool,
        input: Option<String>,
        work: impl Future<Output = Result<Effect, String>> + Send + 'static,
    ) -> bool {
        if !self.has_capacity() {
            self.notice = Some(
                "Action queue full — not accepted; try again after pending actions finish".into(),
            );
            return false;
        }
        self.next_ticket += 1;
        self.queue.push_back(Pending {
            ticket: self.next_ticket,
            origin,
            write,
            input,
            work: Some(Box::pin(work)),
        });
        self.exit_confirm = false;
        self.notice = None;
        self.start();
        true
    }
    fn start(&mut self) {
        if self.task.is_some() {
            return;
        }
        let Some(mut pending) = self.queue.pop_front() else {
            return;
        };
        self.current_ticket = Some(pending.ticket);
        self.current_origin = Some(pending.origin.clone());
        self.current_input = pending.input.clone();
        self.started = Some(tokio::time::Instant::now());
        self.write = pending.write;
        self.warned = false;
        let tx = self.tx.clone();
        self.task = Some(tokio::spawn(async move {
            // Preserve the accepted input/target, not the consumed future.
            let work = pending.work.take().expect("accepted action owns its work");
            let result = if pending.write {
                supervised::work(work).await
            } else {
                supervised::read(work).await
            };
            let _ = tx.send(Completion { pending, result }).await;
        }));
    }
    pub(super) async fn recv(&mut self) -> Option<Completion> {
        self.rx.recv().await
    }
    pub(super) fn warn_if_late(&mut self) -> bool {
        if self.write && !self.warned && self.started.is_some_and(|t| t.elapsed() >= WRITE_WARNING)
        {
            self.warned = true;
            return true;
        }
        false
    }
    /// A second explicit quit authorizes abandoning queued actions, not a
    /// claim that cancelling the started action rolled back its effects.
    pub(super) fn request_exit(&mut self) -> bool {
        if !self.is_pending() {
            return true;
        }
        if self.exit_confirm {
            return true;
        }
        self.exit_confirm = true;
        self.notice = self.exit_warning();
        false
    }
    /// Derived from the same armed quit state in every view, even when an
    /// editor/help footer or a later notice would otherwise hide the warning.
    pub(super) fn exit_warning(&self) -> Option<String> {
        (self.exit_confirm && self.is_pending()).then(|| {
            format!(
                concat!(
                    "Quit again to exit: {} queued action(s) would be cancelled before starting; ",
                    "in-flight {} may already have effects"
                ),
                self.queue.len(),
                if self.write { "write" } else { "read" }
            )
        })
    }
    pub(super) fn exit_report(&self) -> Option<String> {
        let inputs: Vec<_> = self
            .current_input
            .iter()
            .chain(self.queue.iter().filter_map(|p| p.input.as_ref()))
            .chain(self.uncertain_inputs.iter())
            .cloned()
            .collect();
        (self.is_pending() || !inputs.is_empty() || !self.uncertainty.is_empty()).then(|| {
            format!(
                concat!(
                    "Exited with unfinished/unconfirmed actions: queued actions were cancelled before ",
                    "starting; a started write may already have committed. Check the session before retrying.",
                    "\nUnconfirmed accepted inputs:\n{}\nUnresolved outcomes:\n{}"
                ),
                inputs.join("\n---\n"),
                self.uncertainty.join("\n---\n")
            )
        })
    }
}

pub(super) fn cancel_closed_reads(app: &mut App) {
    let valid = |origin: &Origin| {
        origin
            .tab
            .as_ref()
            .is_none_or(|_| origin.tab_index(app).is_some())
    };
    let cancel = !app.operations.write
        && app
            .operations
            .current_origin
            .as_ref()
            .is_some_and(|o| !valid(o));
    let tabs = &app.tabs;
    app.operations.queue.retain(|p| {
        p.write
            || p.origin.tab.as_ref().is_none_or(|(id, inc)| {
                tabs.iter()
                    .any(|t| t.session_db_id == *id && t.live.incarnation == *inc)
            })
    });
    if cancel {
        if let Some(task) = app.operations.task.take() {
            task.abort();
        }
        app.operations.current_ticket = None;
        app.operations.current_origin = None;
        app.operations.started = None;
        app.operations.start();
    }
}

/// Prioritize the accepted creation over historical presentation reads. A
/// metadata open can hold the same account-session lock that database creation needs.
pub(super) fn begin_creation(app: &mut App) -> bool {
    if app.operations.is_creating_session() {
        app.operations.notice =
            Some("Session creation already pending — not accepted again".into());
        return false;
    }
    if !app.backend_ready() || !app.operations.has_capacity() {
        app.operations.notice =
            Some("Backend unavailable or action queue full — creation not accepted".into());
        return false;
    }
    app.cancel_session_fill();
    app.session_list_fresh = false;
    // The catalog reload is deferred until the accepted action settles.
    app.session_catalog_loading = true;
    let ticket = app.operations.next_ticket + 1;
    app.operations.creation_ticket = Some(ticket);
    tracing::info!(ticket, "TUI new session requested");
    true
}

pub(super) fn submit(
    app: &mut App,
    write: bool,
    input: Option<String>,
    work: impl Future<Output = Result<Effect, String>> + Send + 'static,
) {
    if !app.backend_ready() {
        app.operations.notice = Some("Backend unavailable — action not accepted".into());
        return;
    }
    let origin = Origin::capture(app);
    app.operations.submit(origin, write, input, work);
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) enum Invalidation {
    #[default]
    None,
    Session(String),
    All,
}

pub(super) fn refresh_after(
    app: &mut App,
    invalidation: Invalidation,
    tx: &mpsc::Sender<refresh::Refresh>,
) {
    let all = invalidation == Invalidation::All;
    let ids: Vec<_> = match invalidation {
        Invalidation::None => Vec::new(),
        Invalidation::Session(id) => vec![id],
        Invalidation::All => app.tabs.iter().map(|t| t.session_db_id.clone()).collect(),
    };
    for id in ids {
        if all && let Some(i) = app.tab_index_for(&id) {
            app.tabs[i].live.covered = None;
        }
        refresh::request(app, &id, refresh::RefreshKind::Session, tx);
        if app
            .tabs
            .get(app.active_tab)
            .is_some_and(|t| t.session_db_id == id && t.job.is_none())
        {
            refresh::request(app, &id, refresh::RefreshKind::Status, tx);
        }
    }
}

/// In-memory apply only. Writes survive a closed view, but results never
/// mutate a new incarnation or switch focus after newer local navigation.
pub(super) fn apply(app: &mut App, completion: Completion) -> Invalidation {
    if app.operations.current_ticket != Some(completion.pending.ticket) {
        return Invalidation::None;
    }
    app.operations.current_ticket = None;
    app.operations.current_origin = None;
    app.operations.task.take();
    app.operations.started = None;
    app.operations.current_input = None;
    app.operations.warned = false;
    app.operations.exit_confirm = false;
    app.operations.notice = None;
    let Completion { pending, result } = completion;
    let (result, global) = match result {
        Ok(Effect::Global(effect)) => (Ok(*effect), true),
        result => (result, false),
    };
    let nav_valid = pending.origin.navigation_valid(app);
    let target = pending.origin.tab_index(app);
    // Creation stays admitted only while its accepted action continues below
    // (a new destination still preparing); every other settlement releases it.
    let continues = nav_valid
        && matches!(&result, Ok(Effect::Open(open))
            if app.tab_index_for(&open.switch.session_db_id).is_none());
    if app.operations.creation_ticket == Some(pending.ticket) && !continues {
        app.operations.creation_ticket = None;
        app.session_catalog_loading = false;
        app.session_list_fresh = false;
    }
    // Session writes already invalidate through on_write. A scoped check also
    // covers uncertain completions; equality suppression absorbs callback overlap.
    // Never cancel a healthy callback read just because its write completed.
    let mut invalidation = if pending.write {
        if global || result.is_err() || matches!(&result, Ok(Effect::Settings(_))) {
            Invalidation::All
        } else {
            target
                .map(|i| Invalidation::Session(app.tabs[i].session_db_id.clone()))
                .unwrap_or_default()
        }
    } else {
        Invalidation::None
    };
    match result {
        Err(error) => {
            if let Some(i) = target {
                app.tabs[i].live.input_feedback = error.clone();
                app.tabs[i].touch_transcript();
            }
            let qualifier = if pending.write {
                "outcome unknown; check before retrying"
            } else {
                "read failed; cached data retained"
            };
            let input = pending
                .input
                .map(|text| {
                    app.operations.uncertain_inputs.push(text.clone());
                    format!("\nAccepted input: {text}")
                })
                .unwrap_or_default();
            let notice = format!("{error} — {qualifier}{input}");
            if nav_valid {
                if pending.origin.mode == TuiMode::SessionPicker {
                    app.hub_notice = Some(notice.clone());
                }
                if matches!(pending.origin.mode, TuiMode::Settings(_)) {
                    app.settings_status = Some(notice.clone());
                }
            }
            if pending.write {
                app.operations.uncertainty.push(notice);
            } else {
                app.operations.notice = Some(notice);
            }
        }
        Ok(Effect::Open(open)) => {
            if nav_valid {
                if let Some(i) = app.tab_index_for(&open.switch.session_db_id) {
                    app.set_active_tab(i);
                    app.mode = TuiMode::Chat;
                } else {
                    // Continue the same accepted action, not a new submission:
                    // retain its origin/input and run before the next FIFO item.
                    app.operations.queue.push_front(Pending {
                        work: Some(Box::pin(open.prepare())),
                        ..pending
                    });
                }
            }
        }
        Ok(Effect::Opened(tab)) => {
            app.session_list_fresh = false;
            if nav_valid {
                // Notifications delivered before insertion could not find a tab.
                // Check again after insertion, even when its first read succeeded.
                invalidation = Invalidation::Session(tab.session_db_id.clone());
                tracing::info!(ticket = pending.ticket, session_db_id = %tab.session_db_id,
                    agent = %tab.current_agent, "TUI session composer ready");
                if let Some(i) = app.tab_index_for(&tab.session_db_id) {
                    app.set_active_tab(i);
                } else {
                    app.push_tab(*tab);
                }
                app.mode = TuiMode::Chat;
            } else {
                app.operations.notice = Some(format!(
                    "Session {} opened in backend; current view unchanged",
                    tab.title()
                ));
            }
        }
        Ok(Effect::Sent) => {}
        Ok(Effect::JobSent(feedback)) => {
            if let Some(i) = target {
                app.tabs[i].live.input_feedback = feedback;
                app.tabs[i].touch_transcript();
            }
        }
        Ok(Effect::Renamed { id, name }) => {
            // A closed/reopened view receives fresh meta through its own read.
            if let Some(i) = target.filter(|&i| app.tabs[i].session_db_id == id) {
                app.tabs[i].session_name = name.clone();
            }
            if let Some(row) = app.session_list.iter_mut().find(|s| s.session_db_id == id) {
                row.name = name;
            }
        }
        Ok(Effect::Described {
            id,
            description,
            text,
        }) => {
            if let Some(row) = app.session_list.iter_mut().find(|s| s.session_db_id == id) {
                row.description = description;
            }
            show_text(app, target, text);
        }
        Ok(Effect::Settings(text)) => {
            if nav_valid {
                app.settings_status = Some(text);
            } else {
                app.operations.notice = Some(text);
            }
        }
        Ok(Effect::Diff(diff)) => {
            if nav_valid {
                app.agent_diff = Some(diff);
            }
        }
        Ok(Effect::Global(_)) => unreachable!("unwrapped above"),
        Ok(Effect::Outcome(outcome)) => match outcome {
            CommandOutcome::Quit => {
                app.should_quit = app.operations.request_exit();
            }
            CommandOutcome::Text(text) | CommandOutcome::Error(text) => {
                show_text(app, target, text);
            }
            _ => unreachable!("backend outcome must be prepared before application"),
        },
    }
    app.operations.start();
    invalidation
}

/// Show a command result in its originating view, or as a notice without one.
fn show_text(app: &mut App, target: Option<usize>, text: String) {
    if let Some(i) = target {
        app.tabs[i].add_local(SessionEntry {
            sender: "system".into(),
            content: text,
            timestamp: chrono::Utc::now(),
            entry_type: EntryType::Message,
            metadata: None,
            routing: None,
        });
    } else {
        app.operations.notice = Some(text);
    }
}

#[derive(Clone)]
pub(super) struct Target {
    id: String,
    db: eidetica::Database,
    agent: String,
    name: Option<String>,
}
impl Target {
    pub(super) fn capture(app: &App) -> Option<Self> {
        app.current().map(|t| Self {
            id: t.session_db_id.clone(),
            db: t.session_db.clone(),
            agent: t.current_agent.clone(),
            name: t.session_name.clone(),
        })
    }
}

#[derive(Clone)]
pub(super) struct Context {
    pub server: Arc<Server>,
    pub backend: BackendManager,
    pub secrets: SecretStore,
    pub approval_tx: mpsc::Sender<TaggedApproval>,
    pub notify_tx: mpsc::Sender<String>,
}
impl Context {
    pub(super) async fn command(
        self,
        cmd: Command,
        target: Option<Target>,
        selected_info: Option<ModelInfo>,
    ) -> Result<Effect, String> {
        // The value a successful description command stored, so the cached
        // picker row can be patched like a rename instead of going stale.
        let described = match &cmd {
            Command::DescribeSession(text) => {
                Some(chaz_core::session::validate_session_description(text).ok())
            }
            Command::ClearSessionDescription => Some(None),
            _ => None,
        };
        let global = !command_is_read(&cmd)
            && !matches!(
                &cmd,
                Command::NameSession(_)
                    | Command::ClearSessionName
                    | Command::DescribeSession(_)
                    | Command::ClearSessionDescription
                    | Command::Model(_)
                    | Command::AgentModel { .. }
                    | Command::AgentAdd(_)
                    | Command::AgentRemove(_)
                    | Command::AgentSetHost(_)
                    | Command::AgentSetBurst(_)
                    | Command::Stop
                    | Command::RetryInterrupted(_)
                    | Command::Compact
                    | Command::NewSession(_)
                    | Command::SwitchSession(_)
            );
        let target_id = target.as_ref().map(|t| t.id.clone());
        let outcome = if matches!(cmd, Command::SwitchSession(_)) {
            let server = self.server.clone();
            supervised::read(async move {
                commands::dispatch_without_session(cmd, &server)
                    .await
                    .map_err(|_| "Session command needs a conversation".to_owned())
            })
            .await?
        } else {
            match commands::dispatch_without_session(cmd, &self.server).await {
                Ok(outcome) => outcome,
                Err(cmd) => {
                    let mut target = target.ok_or("No conversation is open")?;
                    // Do not turn a dead backend into a default command context.
                    target.name = chaz_core::session::try_read_meta_from_db(&target.db)
                        .await
                        .map_err(|e| e.to_string())?
                        .name;
                    commands::dispatch(
                        cmd,
                        &CommandContext {
                            server: &self.server,
                            secrets: &self.secrets,
                            backend: &self.backend,
                            session_db_id: &target.id,
                            session_db: &target.db,
                            current_agent: &target.agent,
                            session_name: target.name.as_deref(),
                        },
                    )
                    .await
                }
            }
        };
        if let Some(info) = selected_info {
            self.server.cache_model_info(&info).await;
        }
        if let (Some(description), Some(id), CommandOutcome::Text(text)) =
            (described, target_id, &outcome)
        {
            return Ok(Effect::Described {
                id,
                description,
                text: text.clone(),
            });
        }
        let effect = self.prepare(outcome).await?;
        Ok(if global {
            Effect::Global(Box::new(effect))
        } else {
            effect
        })
    }
    async fn prepare(&self, outcome: CommandOutcome) -> Result<Effect, String> {
        match outcome {
            CommandOutcome::SessionSwitched(switch) => Ok(resolved_open(
                &self.server,
                &self.backend,
                self.approval_tx.clone(),
                self.notify_tx.clone(),
                switch,
            )),
            CommandOutcome::SessionsList(list) => Ok(Effect::Outcome(CommandOutcome::Text(
                list.into_iter()
                    .map(|s| format!("{} {:?}", s.session_db_id, s.name))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))),
            CommandOutcome::Error(error) => Err(error),
            outcome => Ok(Effect::Outcome(outcome)),
        }
    }
}

/// Resolution and observation installation have different ownership points.
/// The UI first chooses an existing view or discards stale navigation; only a
/// new accepted destination continues preparation, ahead of later FIFO actions.
pub(super) struct Open {
    server: Arc<Server>,
    backend: BackendManager,
    approval_tx: mpsc::Sender<TaggedApproval>,
    notify_tx: mpsc::Sender<String>,
    switch: Box<chaz_core::commands::SessionSwitch>,
}
pub(super) fn resolved_open(
    server: &Arc<Server>,
    backend: &BackendManager,
    approval_tx: mpsc::Sender<TaggedApproval>,
    notify_tx: mpsc::Sender<String>,
    switch: Box<chaz_core::commands::SessionSwitch>,
) -> Effect {
    Effect::Open(Box::new(Open {
        server: server.clone(),
        backend: backend.clone(),
        approval_tx,
        notify_tx,
        switch,
    }))
}
impl Open {
    async fn prepare(self) -> Result<Effect, String> {
        if chaz_core::session::jobs::is_job_session(&self.switch.db).await {
            return jobs::open_tab(
                &self.server,
                &self.backend,
                &self.switch.session_db_id,
                self.notify_tx,
            )
            .await
            .map(|tab| Effect::Opened(Box::new(tab)));
        }
        let observation = setup_session(
            &self.server,
            &self.switch.db,
            self.backend.clone(),
            self.approval_tx,
            self.notify_tx,
        )
        .await
        .map_err(|e| e.to_string())?;
        let server = self.server.clone();
        let backend = self.backend.clone();
        let db = self.switch.db.clone();
        let id = self.switch.session_db_id.clone();
        let mut tab =
            supervised::read(async move { build_tab(&server, &backend, db, id).await }).await?;
        tab.live.observation = Some(observation);
        Ok(Effect::Opened(Box::new(tab)))
    }
}

pub(super) fn command_is_read(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Info
            | Command::Print
            | Command::ListSessions
            | Command::ListCosts
            | Command::ListAgentGroups
            | Command::Interrupted
            | Command::AgentsList
            | Command::AgentRoom
            | Command::ListChannels
            | Command::Model(None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, atomic::AtomicUsize};
    use tokio::sync::oneshot;

    async fn app() -> App {
        let (_instance, mut user) = eidetica::Instance::create_backend(
            Box::new(eidetica::backend::database::InMemory::new()),
            eidetica::NewAccount::passwordless("actions"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        App::new(
            HashSet::new(),
            Tab {
                job: None,
                session_db_id: "a".into(),
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
                model_pin: None,
                expanded_entries: HashSet::new(),
                live: Default::default(),
            },
        )
    }
    async fn complete(app: &mut App) {
        let msg = app.operations.recv().await.unwrap();
        apply(app, msg);
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[tokio::test]
    async fn escape_confirms_accepted_work_but_first_dismisses_completion() {
        let mut app = app().await;
        submit(
            &mut app,
            true,
            Some("accepted".into()),
            std::future::pending(),
        );
        submit(
            &mut app,
            true,
            Some("queued".into()),
            std::future::pending(),
        );
        app.input = "/".into();
        app.cursor = 1;
        input::recompute_completion(&mut app);
        assert!(app.completion.is_some());
        input::handle_chat_key(&mut app, key(KeyCode::Esc));
        assert!(!app.should_quit && app.completion.is_none());
        assert_eq!(app.input, "/");
        input::handle_chat_key(&mut app, key(KeyCode::Esc));
        assert!(!app.should_quit, "Esc silently abandoned accepted work");
        assert!(app.operations.is_pending());
        assert!(app.operations.indicator().contains("again to exit"));
        input::handle_chat_key(&mut app, key(KeyCode::Esc));
        assert!(app.should_quit, "explicit second exit was not honored");
        assert!(app.operations.exit_report().unwrap().contains("queued"));
    }

    #[tokio::test]
    async fn transient_notice_clears_on_acceptance_without_losing_write_uncertainty() {
        let mut app = app().await;
        app.backend_available = false;
        submit(&mut app, true, None, async { Ok(Effect::Sent) });
        assert!(app.operations.indicator().contains("not accepted"));
        app.backend_available = true;
        submit(&mut app, true, Some("uncertain".into()), async {
            Err("lost write".into())
        });
        assert!(
            !app.operations.indicator().contains("not accepted"),
            "rejection masked accepted work"
        );
        complete(&mut app).await;
        assert!(app.operations.indicator().contains("outcome unknown"));
        app.operations.notice = Some("Backend reconnected; writes were not replayed".into());
        submit(&mut app, true, None, async { Ok(Effect::Sent) });
        assert!(!app.operations.indicator().contains("reconnected"));
        complete(&mut app).await;
        assert!(app.operations.indicator().contains("outcome unknown"));
        assert!(app.operations.exit_report().unwrap().contains("uncertain"));
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_actions_are_ordered_and_a_late_write_is_not_cancelled_or_retried() {
        let mut app = app().await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let (release, gate) = oneshot::channel();
        let first = order.clone();
        submit(&mut app, true, Some("accepted-界".into()), async move {
            first.lock().unwrap().push(1);
            gate.await.unwrap();
            Ok(Effect::Sent)
        });
        for i in [2, 3] {
            let order = order.clone();
            submit(&mut app, true, None, async move {
                order.lock().unwrap().push(i);
                Ok(Effect::Sent)
            });
        }
        tokio::task::yield_now().await;
        for c in "second-é界".chars() {
            input::handle_chat_key(&mut app, key(KeyCode::Char(c)));
        }
        input::handle_chat_key(&mut app, key(KeyCode::PageUp));
        assert_eq!(app.input, "second-é界");
        assert_eq!(app.active().scroll_offset, 20);
        tokio::time::advance(WRITE_WARNING * 2).await;
        assert!(app.operations.warn_if_late());
        assert!(app.operations.indicator().contains("outcome unknown"));
        assert_eq!(
            *order.lock().unwrap(),
            [1],
            "no next action, rollback or retry at deadline"
        );
        assert!(
            !app.operations.request_exit(),
            "first quit must not discard accepted work"
        );
        release.send(()).unwrap();
        for _ in 0..3 {
            complete(&mut app).await;
        }
        assert_eq!(*order.lock().unwrap(), [1, 2, 3]);
        assert_eq!(app.input, "second-é界");
        assert!(app.operations.request_exit());
    }

    #[tokio::test]
    async fn bounded_admission_retains_a_saturated_composer() {
        let mut app = app().await;
        for _ in 0..CAPACITY {
            submit(&mut app, true, None, std::future::pending());
        }
        app.input = "not accepted-界".into();
        app.cursor = app.input.len();
        assert!(input::handle_chat_key(&mut app, key(KeyCode::Enter)).is_none());
        assert_eq!(app.input, "not accepted-界");
        assert!(!app.operations.has_capacity());
        assert!(
            app.operations
                .notice
                .as_ref()
                .unwrap()
                .contains("not submitted")
        );
    }

    #[tokio::test]
    async fn closed_reopened_and_cross_tab_results_never_mutate_the_wrong_view() {
        let mut app = app().await;
        submit(&mut app, true, None, async {
            Ok(Effect::Outcome(CommandOutcome::Text("result-of-a".into())))
        });
        let msg = app.operations.recv().await.unwrap();
        let db = app.active().session_db.clone();
        let mut b = self::app().await; // independent tab with the same literal id below
        b.active_mut().session_db_id = "b".into();
        app.push_tab(b.tabs.remove(0));
        apply(&mut app, msg);
        assert_eq!(app.tabs[0].entries[0].content, "result-of-a");
        assert!(app.active().entries.is_empty());
        app.set_active_tab(0);
        submit(&mut app, true, None, async {
            Ok(Effect::Outcome(CommandOutcome::Text(
                "old-incarnation".into(),
            )))
        });
        let msg = app.operations.recv().await.unwrap();
        app.close_tab_at(0);
        let mut reopened = self::app().await;
        reopened.active_mut().session_db = db;
        app.push_tab(reopened.tabs.remove(0));
        apply(&mut app, msg);
        assert!(app.active().entries.is_empty());
        assert_eq!(app.operations.notice.as_deref(), Some("old-incarnation"));
    }

    #[tokio::test]
    async fn errors_preserve_accepted_input_and_exit_drops_owned_work() {
        let mut app = app().await;
        submit(&mut app, true, Some("uncertain input".into()), async {
            Err("connection lost after submit".into())
        });
        complete(&mut app).await;
        assert!(
            app.operations
                .exit_report()
                .unwrap()
                .contains("uncertain input")
        );
        assert!(app.operations.indicator().contains("outcome unknown"));
        let drops = Arc::new(AtomicUsize::new(0));
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let guard = Guard(drops.clone());
        submit(&mut app, true, Some("in flight".into()), async move {
            let _guard = guard;
            std::future::pending().await
        });
        tokio::task::yield_now().await;
        assert!(!app.operations.request_exit());
        assert!(app.operations.request_exit());
        assert!(app.operations.exit_report().unwrap().contains("in flight"));
        drop(app);
        tokio::task::yield_now().await;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn closing_a_tab_cancels_its_read_but_not_its_accepted_write() {
        let mut app = app().await;
        submit(&mut app, false, None, std::future::pending());
        app.close_tab_at(0);
        cancel_closed_reads(&mut app);
        assert!(!app.operations.is_pending());
        let mut app = self::app().await;
        let (release, gate) = oneshot::channel();
        submit(&mut app, true, Some("accepted".into()), async move {
            gate.await.unwrap();
            Ok(Effect::Sent)
        });
        app.close_tab_at(0);
        cancel_closed_reads(&mut app);
        assert!(app.operations.is_pending());
        release.send(()).unwrap();
        complete(&mut app).await;
        assert!(!app.operations.is_pending());
    }
    #[tokio::test]
    async fn local_commands_remain_available_offline_and_with_a_full_action_queue() {
        let mut app = app().await;
        app.backend_available = false;
        app.input = "/settings".into();
        app.cursor = app.input.len();
        assert!(
            matches!(
                input::handle_chat_key(&mut app, key(KeyCode::Enter)),
                Some(ChatAction::OpenSettings(SettingsScope::Session))
            ),
            "cached Settings command was blocked by backend admission"
        );
        app.input = "/expand".into();
        app.cursor = app.input.len();
        input::handle_chat_key(&mut app, key(KeyCode::Enter));
        assert!(app.expand_all);
        app.backend_available = true;
        for _ in 0..CAPACITY {
            submit(&mut app, true, None, std::future::pending());
        }
        app.input = "/settings".into();
        app.cursor = app.input.len();
        assert!(matches!(
            input::handle_chat_key(&mut app, key(KeyCode::Enter)),
            Some(ChatAction::OpenSettings(SettingsScope::Session))
        ));
        app.input = "/quit".into();
        app.cursor = app.input.len();
        assert!(matches!(
            input::handle_chat_key(&mut app, key(KeyCode::Enter)),
            Some(ChatAction::Dispatch(Command::Quit))
        ));
    }

    #[tokio::test]
    async fn refused_backend_submission_keeps_cursor_and_completion_state() {
        let mut app = app().await;
        app.backend_available = false;
        app.input = "/name not-submitted".into();
        app.cursor = 8;
        input::recompute_completion(&mut app);
        app.completion_dismissed = true;
        assert!(input::handle_chat_key(&mut app, key(KeyCode::Enter)).is_none());
        assert_eq!(app.input, "/name not-submitted");
        assert_eq!(app.cursor, 8);
        assert!(app.completion_dismissed);
    }
}
