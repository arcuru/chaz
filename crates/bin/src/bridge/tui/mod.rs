//! Terminal UI bridge. Elm-style architecture:
//! - `App` holds global UI state (mode, overlay, input, click regions, tab
//!   list) plus a `Vec<Tab>` where each `Tab` owns one session's state
//!   (entries, scroll, pending approval, session DB handle, etc.).
//! - `Action` is the update message.
//! - `view::ui` renders a frame from `&mut App`.
//! - `input::parse_chat_line` turns typed text into `ChatAction`s.
//!
//! Submodules:
//! - `input` — KeyEvent / MouseEvent handling, slash-command parsing
//! - `view`  — ratatui rendering

use chaz_core::backends::{BackendManager, ModelInfo};
use chaz_core::bridge::{ApprovalExchange, Bridge};
use chaz_core::commands::{self, Command, CommandContext, CommandOutcome, SessionInfo};
use chaz_core::config::Config;
use chaz_core::security::SecretStore;
use chaz_core::server::Server;
use chaz_core::session::SessionIndex;
use chaz_core::session::{AgentRef, EntryType, Session, SessionEntry, SessionMeta};

use std::collections::HashMap;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_stream::StreamExt;

mod bridge_impl;
mod input;
mod theme;
mod view;
mod widgets;

pub struct TuiBridge {
    config: Config,
    secrets: SecretStore,
    /// Optional text to pre-fill the composer with on launch, unsent. Mirrors
    /// claude/pi: `chaz "hello"` opens a conversation with "hello" already in
    /// the composer so the user can review and hit Enter. Without `session`,
    /// launch creates one new conversation for it instead of showing the hub.
    initial_prompt: Option<String>,
    /// `--session NAME`: open that named session (creating it when absent)
    /// instead of the hub.
    session: Option<String>,
}

impl TuiBridge {
    pub fn new(config: Config, secrets: SecretStore) -> Self {
        Self {
            config,
            secrets,
            initial_prompt: None,
            session: None,
        }
    }

    pub fn with_initial_prompt(mut self, prompt: String) -> Self {
        self.initial_prompt = Some(prompt);
        self
    }

    pub fn with_session(mut self, name: String) -> Self {
        self.session = Some(name);
        self
    }
}

/// Approval routed from the server through a per-tab forwarder, tagged with
/// the owning session DB ID so the TUI knows which tab to show the prompt on.
pub(super) type TaggedApproval = (String, ApprovalExchange);

enum Action {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// A session DB fired an on_write callback — payload is the
    /// session_db_id so we can refresh the right tab.
    SessionChanged(String),
    ApprovalRequest(TaggedApproval),
    /// A background catalog fetch finished. `Ok` carries the live model
    /// list (already merged with cache); `Err` carries a display message.
    ModelsFetched(Result<Vec<ModelInfo>, String>),
    SessionLoad(SessionLoad),
}

enum SessionLoad {
    Catalog {
        generation: u64,
        result: Result<Vec<SessionIndex>, String>,
    },
    Row {
        generation: u64,
        info: SessionInfo,
    },
}

enum CatalogLoadOutcome {
    Ignored,
    Loaded,
    Failed,
}

/// Handle to in-flight picker work. Metadata reads are demand-driven and
/// bounded to the visible window; cancellation plus generation checks keep
/// obsolete results from mutating a later picker.
pub(super) struct SessionFill {
    generation: u64,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TuiMode {
    Chat,
    SessionPicker,
    ModelPicker,
    Settings(SettingsScope),
}

/// Which DB / domain a Settings page is editing. Two distinct surfaces:
/// `Peer` edits `chaz_peer` + config-derived globals; `Session` edits the
/// active tab's `SessionMeta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsScope {
    Peer,
    Session,
}

/// Category → List → Content input ownership. Static pages skip List;
/// Tab / BackTab always return to Category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsFocus {
    Category,
    List,
    Content,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct SettingsReaderIdentity {
    scope: SettingsScope,
    category: usize,
    entity: Option<String>,
}

/// One reading position, not a cache of positions for previously visited entities.
#[derive(Default)]
pub(super) struct SettingsReader {
    identity: Option<SettingsReaderIdentity>,
    pub offset: u16,
    pub max_offset: u16,
    pub visible_rows: u16,
    pub viewport: Option<ratatui::layout::Rect>,
}

impl SettingsReader {
    pub fn scroll(&mut self, down: bool, lines: u16) {
        self.offset = if down {
            self.offset.saturating_add(lines).min(self.max_offset)
        } else {
            self.offset.saturating_sub(lines)
        };
    }
}

/// Categories listed in the Peer Settings sidebar. Ordering here is the
/// display order. Stage 1 leaves every category as a `(coming soon)`
/// placeholder; subsequent stages fill in the detail panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PeerSettingsCategory {
    Agents,
    Backends,
    Defaults,
    Bridges,
    Extensions,
    Mcp,
    Groups,
    Identity,
    About,
}

impl PeerSettingsCategory {
    pub(super) const ALL: &'static [Self] = &[
        Self::Agents,
        Self::Backends,
        Self::Defaults,
        Self::Bridges,
        Self::Extensions,
        Self::Mcp,
        Self::Groups,
        Self::Identity,
        Self::About,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Agents => "Agents",
            Self::Backends => "Backends",
            Self::Defaults => "Defaults",
            Self::Bridges => "Bridges",
            Self::Extensions => "Extensions",
            Self::Mcp => "MCP",
            Self::Groups => "Groups",
            Self::Identity => "Identity",
            Self::About => "About",
        }
    }
}

/// Categories listed in the Session Settings sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionSettingsCategory {
    Overview,
    Agents,
    Models,
    Routing,
    History,
    Sharing,
}

impl SessionSettingsCategory {
    pub(super) const ALL: &'static [Self] = &[
        Self::Overview,
        Self::Agents,
        Self::Models,
        Self::Routing,
        Self::History,
        Self::Sharing,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Agents => "Agents",
            Self::Models => "Models",
            Self::Routing => "Routing",
            Self::History => "History",
            Self::Sharing => "Sharing",
        }
    }
}

/// A scope tab in the model picker. The active scope decides where the
/// selected model gets written: `Session` updates `SessionMeta.model` via
/// `Command::Model`; `Agent` updates `SessionMeta.agent_models[name]` via
/// `Command::AgentModel`. Built when the picker opens from the current
/// session's attached agents; `Session` is always present and pinned first.
#[derive(Clone, Debug)]
pub(super) enum ModelPickerScope {
    Session,
    Agent(String),
    /// Agent-level (DB) scope — writes to the agent's `AgentDbConfig.model`,
    /// not the session meta. Used from Peer→Agents detail Enter.
    AgentGlobal(String),
}

impl ModelPickerScope {
    pub(super) fn label(&self) -> &str {
        match self {
            ModelPickerScope::Session => "Session",
            ModelPickerScope::Agent(name) | ModelPickerScope::AgentGlobal(name) => name,
        }
    }
}

/// Bottom-strip inline edit prompt active inside a Settings page. When
/// `Some`, the status strip slot is replaced by the edit widget and
/// keystrokes route to the prompt instead of category navigation. On
/// Enter the main loop dispatches the appropriate command based on
/// `intent` and clears the slot.
pub(super) struct SettingsPrompt {
    pub label: String,
    pub input: String,
    pub cursor: usize,
    pub intent: SettingsPromptIntent,
}

/// What the active settings prompt is collecting. Each variant is a
/// distinct edit operation; the main loop dispatches on this when the
/// user hits Enter.
#[derive(Clone, Copy, Debug)]
pub(super) enum SettingsPromptIntent {
    /// Add an agent (by display name or DB id) to the active session.
    /// Translates to `Command::AgentAdd` on submit.
    AddSessionAgent,
    /// Append an agent name to the persisted `default_agents` list.
    /// Validated against the registered agent names; rejected entries
    /// surface in `settings_status`.
    AddPeerDefault,
}

/// Bottom-strip filter-as-you-type picker active inside a Settings page.
/// `Some` while the user is choosing an item from a known list; mutually
/// exclusive with `settings_prompt`. On Enter the highlighted candidate
/// dispatches the same `PromptSubmit` arm as the freeform prompt, keyed
/// by `intent`.
pub(super) struct SettingsPicker {
    pub label: String,
    pub filter: String,
    pub cursor: usize,
    pub candidates: Vec<String>,
    pub selected: usize,
    pub intent: SettingsPickerIntent,
    /// Match-row viewport from the last frame; hidden pickers own no hits.
    pub viewport: ratatui::layout::Rect,
}

/// What the active settings picker is collecting. Same shape as
/// `SettingsPromptIntent` but separate so the type system enforces that
/// only intents with a known candidate list reach the picker path.
#[derive(Clone, Copy, Debug)]
pub(super) enum SettingsPickerIntent {
    /// Pick an agent from the peer registry to add to the active session.
    /// Translates to `Command::AgentAdd` on submit, via the shared
    /// `PromptSubmit { AddSessionAgent }` arm.
    AddSessionAgent,
}

impl SettingsPicker {
    /// Indices into `candidates` whose entries case-insensitively contain
    /// `filter`. With empty filter this is `0..candidates.len()`.
    pub fn filtered(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.candidates.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.candidates
            .iter()
            .enumerate()
            .filter(|(_, name)| name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// Currently highlighted candidate name, if any.
    pub fn selected_name(&self) -> Option<&str> {
        let filtered = self.filtered();
        filtered
            .get(self.selected)
            .and_then(|i| self.candidates.get(*i))
            .map(|s| s.as_str())
    }
}

/// Peer→Agents yaml↔DB diff/merge modal. Present (`App::agent_diff` is
/// `Some`) while the user is inspecting one agent's drift and choosing a
/// merge. Mutually exclusive with the prompt/picker — opening it from the
/// Peer→Agents list takes over the detail pane. The actual diff data is
/// computed server-side ([`Server::agent_diff`]); this just holds the
/// snapshot plus per-row pick state for the interactive `[a]` flow.
pub(super) struct AgentDiffView {
    /// Display name of the agent being diffed.
    pub agent_name: String,
    /// Field + worker diff snapshot taken when the view opened.
    pub diff: chaz_core::agent_diff::AgentDiff,
    /// Which sub-mode is active (plain view / per-field pick / reseed confirm).
    pub mode: AgentDiffMode,
    /// Row cursor — indexes `diff.rows` (the mergeable field rows only;
    /// workers are display-only and not cursorable).
    pub cursor: usize,
    /// Per-row accept flags for `[a]` pick mode. `picks.len() == diff.rows.len()`.
    /// Seeded to the changed rows so Enter-without-toggling applies the drift.
    pub picks: Vec<bool>,
}

/// Sub-mode of the [`AgentDiffView`]. Plain `View` lists the diff; `Pick`
/// adds per-row checkboxes for `[a]`; `ConfirmReseed` gates the destructive
/// `[R]` full overwrite behind a y/n.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AgentDiffMode {
    View,
    Pick,
    ConfirmReseed,
}

/// Frozen view of an active session's meta + a few cached derivatives, taken
/// at the moment Session Settings opens. Keeps render code synchronous —
/// reading `SessionMeta` requires an `async` round-trip into the session DB.
/// Refreshed on `Action::SessionChanged` for the active tab so edits made
/// elsewhere (or via the Models passthrough) propagate without manual reload.
pub(super) struct SessionMetaSnapshot {
    pub session_db_id: String,
    pub model_pin: Option<String>,
    pub agent_models: HashMap<String, String>,
    pub agents: Vec<AgentRef>,
    pub host_agent_db_id: Option<String>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub entry_count: usize,
}

pub(super) enum ChatAction {
    Dispatch(Command),
    OpenPicker,
    /// Open the model picker scoped to `scope`. From `/models` and
    /// Session Settings → Models row Enter; the scope is decided by
    /// the caller and locked for the picker's lifetime.
    OpenModelPicker {
        scope: ModelPickerScope,
    },
    /// Open the Settings page in the given scope. From chat this is always
    /// `Session`; the `/settings` command in chat dispatches it that way.
    OpenSettings(SettingsScope),
    /// Open Session Settings landed on the Models page. Distinct from
    /// `OpenSettings(Session)` because `/models` shouldn't disturb
    /// `session_settings_index` for someone who navigated away from
    /// Models on a previous trip — it always jumps to the Models
    /// category.
    OpenModelsSettings,
    SendMessage(String),
}

pub(super) enum Overlay {
    Help {
        scroll: u16,
    },
    /// Modal input for renaming a session. Submitting an empty string clears
    /// the alias (matches `/name` with no arg).
    RenamePrompt {
        session_db_id: String,
        title: String,
        input: String,
        cursor: usize,
    },
}

/// Inline slash-command completion popup state. Present only while the input
/// starts with `/` and at least one catalog command prefix-matches it (and the
/// user hasn't dismissed it with Esc for the current input).
pub(super) struct Completion {
    /// `(template, description)` pairs from the command catalog whose template
    /// prefix-matches the current input, case-insensitively.
    pub matches: Vec<(&'static str, &'static str)>,
    /// Index into `matches` of the highlighted row.
    pub selected: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ClickTarget {
    OverlayDismiss,
    HelpCommand(&'static str),
    /// Accept completion row `i` into the input box.
    CompletionSelect(usize),
    ApprovalApprove,
    ApprovalDeny,
    ApprovalApproveAll,
    /// Select session-list row `i` (display index is `i + 1` — the New
    /// session row is row 0).
    PickerSelect(usize),
    /// The virtual "New session" row at the top of the picker.
    PickerNew,
    /// Activate tab at the given index.
    TabActivate(usize),
    /// Close tab at the given index.
    TabClose(usize),
    /// Flip the per-entry expand override on the active tab's entry at the
    /// given index. Inverts against `App::expand_all`, so the click always
    /// produces the opposite of whatever's currently rendered.
    ToggleEntryExpanded(usize),
    /// Select model picker row `i` (index into `App::model_list`).
    ModelPickerSelect(usize),
    /// Jump to Settings sidebar category `i` and pin focus to the sidebar.
    /// Index is into `PeerSettingsCategory::ALL` or `SessionSettingsCategory::ALL`
    /// depending on the active scope.
    SettingsSidebarItem(usize),
    /// Click inside the active category's inner list — sets focus to the
    /// detail pane and moves the per-category cursor to row `i`. Only
    /// emitted for categories that own an inner list.
    SettingsDetailRow(usize),
    /// Focus the visible read-only body without changing its selected entity.
    SettingsContent,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ClickRegion {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub target: ClickTarget,
}

impl ClickRegion {
    pub fn hit(&self, col: u16, row: u16) -> bool {
        col >= self.x && col < self.x + self.w && row >= self.y && row < self.y + self.h
    }
}

/// Per-session state. Each `Tab` wraps one eidetica session database plus the
/// UI state specific to viewing it (scroll position, pending approval, etc.).
pub(super) struct Tab {
    pub session_db_id: String,
    pub session_db: eidetica::Database,
    pub entries: Vec<SessionEntry>,
    pub scroll_offset: u16,
    pub pending_approval: Option<ApprovalExchange>,
    pub active_turns: usize,
    pub current_agent: String,
    pub session_name: Option<String>,
    /// The model the runtime would actually use for this session's next
    /// turn, as resolved from session pins and the current agent/backend default.
    /// Refreshed on every draw; empty when no backends are configured.
    pub effective_model: String,
    /// Full roster of agents attached to this session, each with its
    /// resolved effective model and whether it is the designated host.
    /// Drives the multi-agent status-bar segment; for a single-agent
    /// session the bar falls back to `current_agent`/`effective_model` so
    /// its rendering stays byte-identical. Refreshed on session changes;
    /// the current agent's model also refreshes on draw.
    pub roster: Vec<RosterAgent>,
    /// The per-turn context budget (tokens) the runtime would target for the
    /// current agent's effective model — the model's resolved window, lowered
    /// by any per-agent cap, or the configured default when the window is
    /// unknown. Denominator for the status bar's numeric context pair. Refreshed on
    /// every draw from the runtime overlay and current agent cap.
    pub context_budget: usize,
    /// Session/per-agent pin, retaining any backend prefix. Kept separate from
    /// defaults so live agent edits can re-resolve unpinned models on draw.
    pub model_pin: Option<String>,
    /// Per-entry expand override (entry index → "opposite of `App::expand_all`").
    /// Empty by default; click on an entry's icon toggles its presence here.
    pub expanded_entries: HashSet<usize>,
}

/// One attached agent as shown in the multi-agent status bar: its display
/// name, the model the runtime would actually use for it (per-agent pin →
/// session pin → agent default → backend default), and whether it is the
/// session's host agent (the un-mentioned-message responder).
#[derive(Clone)]
pub(super) struct RosterAgent {
    pub name: String,
    pub model: String,
    pub is_host: bool,
}

/// Build the status-bar roster for a session from its `SessionMeta`,
/// resolving each attached agent's effective model the same way the live
/// turn does (`SessionMeta::resolve_model_for_agent` → agent `default_model`
/// → `BackendManager::resolve_model_name`).
fn build_roster(server: &Server, backend: &BackendManager, meta: &SessionMeta) -> Vec<RosterAgent> {
    meta.agents
        .iter()
        .map(|a| {
            let agent_default = server
                .agents()
                .get(&a.display_name)
                .and_then(|ag| ag.default_model.clone());
            let session_model = meta
                .resolve_model_for_agent(&a.display_name)
                .map(str::to_string);
            let model =
                backend.resolve_model_name(session_model.as_deref().or(agent_default.as_deref()));
            RosterAgent {
                name: a.display_name.clone(),
                model,
                is_host: meta.host_agent_db_id.as_deref() == Some(a.db_id.as_str()),
            }
        })
        .collect()
}

/// Short, human-distinguishable form of a session DB id, used for tab
/// titles, the status bar, and the picker. Session ids share a long common
/// leading prefix, so the first characters are useless for telling sessions
/// apart — show the *trailing* characters (the part that actually differs),
/// marked with a leading `…` so it's clear it's truncated.
pub(super) fn short_session_id(s: &str) -> String {
    let tail = s.rsplit(':').next().unwrap_or(s);
    let n = tail.chars().count();
    if n <= 8 {
        tail.to_string()
    } else {
        let suffix: String = tail.chars().skip(n - 8).collect();
        format!("…{suffix}")
    }
}

impl Tab {
    /// Title shown on the tab bar — session name if set, else a short id.
    pub fn title(&self) -> String {
        match &self.session_name {
            Some(name) => name.clone(),
            None => short_session_id(&self.session_db_id),
        }
    }
}

/// Refresh `app.status_segments` from the active session's `extension_outputs`
/// store (the daemon writes it at the turn boundary). Flattens every
/// extension's status map into render-ready segment values, ordered by
/// extension name then key. No-op when there's no active tab.
pub(super) async fn refresh_status_segments(app: &mut App) {
    let Some(db) = app.tabs.get(app.active_tab).map(|t| t.session_db.clone()) else {
        return;
    };
    let outputs = chaz_core::extension::read_extension_outputs(&db).await;
    app.status_segments = outputs
        .into_values()
        .flat_map(|o| o.status.into_values())
        .collect();
}

/// Width of the Settings category rail; the detail pane starts right of it.
pub(super) const SETTINGS_SIDEBAR_W: u16 = 16;

/// First visible row of each cursor-driven list in the TUI.
///
/// Held here rather than derived from the cursor so a list keeps its place
/// while the cursor moves inside the window — deriving it would snap the
/// window to the cursor on every keypress. Each list clamps its own entry to
/// the row count it just drew, so a list that shrinks self-corrects without an
/// explicit reset.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct ListScroll {
    /// Row index into `App::picker_len()`; 0 is the virtual "New session" row.
    pub(super) picker: usize,
    /// Peer → Defaults agent list.
    pub(super) peer_defaults: usize,
    /// Peer → Agents list.
    pub(super) peer_agents: usize,
    /// Peer → MCP server list.
    pub(super) peer_mcp: usize,
    /// Session → Agents list.
    pub(super) session_agents: usize,
    /// Session → Models scope list.
    pub(super) session_models: usize,
}

pub(super) struct App {
    pub(super) mode: TuiMode,
    pub(super) overlay: Option<Overlay>,
    pub(super) click_regions: Vec<ClickRegion>,
    pub(super) input: String,
    pub(super) cursor: usize,
    /// Active slash-command completion popup, if any. Recomputed on every
    /// input edit (see `input::recompute_completion`).
    pub(super) completion: Option<Completion>,
    /// Set when the user dismisses the popup with Esc; suppresses re-opening
    /// until the input is edited again.
    pub(super) completion_dismissed: bool,
    pub(super) tabs: Vec<Tab>,
    pub(super) active_tab: usize,
    pub(super) agent_names: HashSet<String>,
    pub(super) should_quit: bool,
    pub(super) debug_mode: bool,
    /// Mirror of `Server::is_startup_ready`, refreshed each frame in `ui`.
    /// Drives the "⟳ reconciling agents…" status indicator on the fast-start
    /// path; the deferred at-startup work clears it once the gate opens.
    /// Defaults `true` so steady-state and tests show no indicator.
    pub(super) startup_ready: bool,
    /// When true, tool calls / tool results / directives render their full
    /// content. When false (default), they collapse to a one-line summary.
    /// Toggled by Ctrl+T or `/expand`.
    pub(super) expand_all: bool,
    pub(super) session_list: Vec<SessionInfo>,
    /// Whether the current catalog snapshot is reusable on the next open.
    pub(super) session_list_fresh: bool,
    /// Catalog rows retained by id so visible metadata requests never need to
    /// re-list the catalog.
    pub(super) session_indices: HashMap<String, SessionIndex>,
    pub(super) session_fill: Option<SessionFill>,
    pub(super) session_generation: u64,
    pub(super) session_catalog_loading: bool,
    pub(super) session_picker_error: Option<String>,
    /// One-shot result shown in the hub footer (a failed open or create);
    /// cleared by the next hub key.
    pub(super) hub_notice: Option<String>,
    /// Unsent composer text and cursor of each open conversation that is not
    /// focused. The focused one lives in `input`/`cursor`.
    pub(super) drafts: HashMap<String, (String, usize)>,
    pub(super) session_rows_loading: HashSet<String>,
    pub(super) session_metadata_limit: Arc<Semaphore>,
    pub(super) picker_index: usize,
    /// Session rows actually visible in the last drawn picker frame. Requests
    /// run after drawing so input, catalog updates and resizes cannot use stale geometry.
    pub(super) picker_visible_rows: std::ops::Range<usize>,
    /// Scroll offsets for the cursor-driven lists — see [`ListScroll`].
    pub(super) scroll: ListScroll,
    /// Sorted snapshot of the model picker's contents — favorites
    /// (YAML-configured) followed by the live OpenRouter catalog when
    /// available. Repopulated when the picker opens. Sort order: current
    /// effective model first, then favorites alphabetical, then catalog
    /// alphabetical (catalog entries that duplicate a favorite id are
    /// dropped).
    pub(super) model_list: Vec<ModelInfo>,
    /// In-memory cache of the live provider catalog for this session, set the
    /// first time the picker pulls it and reused on subsequent opens (instant,
    /// no re-fetch). Never persisted — that's the whole point of the in-use
    /// model store. Cleared by the picker's refresh binding to force a re-pull;
    /// gone entirely on restart, where the next open re-fetches.
    pub(super) session_catalog: Option<Vec<ModelInfo>>,
    /// Index into `model_picker_filtered`, NOT into `model_list`. Resolved
    /// to a `ModelInfo` via `model_list[model_picker_filtered[idx]]`.
    pub(super) model_picker_index: usize,
    /// Indices into `model_list` that survive the current `model_search`
    /// filter, ordered by fuzzy-match score (best first) when there's a
    /// query, or in `model_list` order when the query is empty.
    pub(super) model_picker_filtered: Vec<usize>,
    /// Top row of the visible scroll window into `model_picker_filtered`.
    /// Clamped each frame to keep the selected row visible.
    pub(super) model_picker_scroll: u16,
    /// Live fuzzy-search query. Edited in place by typing in the picker;
    /// matched against the searchable text of each model (id + capability
    /// labels like "vision audio image-gen").
    pub(super) model_search: String,
    /// Reusable nucleo matcher — keeps internal scratch buffers across
    /// keystrokes so per-character recompute stays cheap.
    pub(super) model_picker_matcher: nucleo_matcher::Matcher,
    /// YAML-configured models held aside so a force-refresh doesn't
    /// briefly drop them from the visible list while the network call is
    /// in flight. Catalog enrichment patches missing prices/capabilities
    /// on these favorites by id-matching against the live catalog before
    /// the merged `model_list` is rebuilt.
    pub(super) model_picker_favorites: Vec<ModelInfo>,
    /// True while a background `/models` fetch is in flight. Picker shows
    /// a "Loading…" hint and the catalog rows haven't arrived yet.
    pub(super) model_picker_loading: bool,
    /// Set when the last fetch failed; cleared on a successful retry.
    pub(super) model_picker_error: Option<String>,
    /// Scope the picker is editing. Set at open time from the caller
    /// (`/models` → Models page → row cursor). Decides whether Enter
    /// dispatches `Command::Model` (session-wide pin) or
    /// `Command::AgentModel` (per-agent override). Defaults to
    /// `Session` outside an open picker.
    pub(super) model_picker_scope: ModelPickerScope,
    /// Pin snapshot for the active scope, taken when the picker opened.
    /// Drives the `(current)` annotation on the matching list row
    /// without re-reading meta on every keystroke. `None` when the
    /// scope has no pin.
    pub(super) model_picker_current_pin: Option<String>,
    /// Snapshot of the active session's `SessionMeta` taken when Session
    /// Settings opens (and refreshed on `Action::SessionChanged` for the
    /// active tab). Lets the Session-side category renderers read the meta
    /// without doing async work mid-frame. `None` outside Session Settings.
    pub(super) session_settings_snapshot: Option<SessionMetaSnapshot>,
    /// Sub-cursor inside the Peer → Agents list. Cycles with ↑↓ while
    /// that category is selected; clamped each frame to the live agent
    /// count. Persists across category switches so the user lands back
    /// where they were.
    pub(super) peer_agents_cursor: usize,
    /// Sorted snapshot of `server.agents().names()` refreshed at the
    /// top of each `view::ui` frame while Peer Settings is up. The
    /// Peer→Agents view and the input handler both index into this so
    /// the cursor row always points at the same agent in both places.
    pub(super) peer_agents_names: Vec<String>,
    /// Snapshot of `server.default_agents()` for the Peer→Defaults
    /// editor. Refreshed at the top of each frame so DB writes /
    /// `set_default_agents` calls show up live. Order is the persisted
    /// order — first entry is the routing host.
    pub(super) peer_defaults: Vec<String>,
    /// Sub-cursor inside the Peer → Defaults list. Clamped to live
    /// length each render; persists across category switches.
    pub(super) peer_defaults_cursor: usize,
    /// Snapshot of `server.mcp_registry().snapshot()` refreshed at the
    /// top of each frame while Peer Settings is up. Sorted by server
    /// name. The Peer→MCP view and input handler both index into this
    /// so the cursor row always points at the same server in both
    /// places.
    pub(super) peer_mcp_servers: Vec<chaz_core::mcp::McpRegistryEntry>,
    /// Extension status segments flattened for the status strip, refreshed
    /// by the run loop from the active session's `extension_outputs` store (the
    /// daemon writes it at the turn boundary). Each entry is one
    /// ready-to-render segment value, ordered by extension then key.
    pub(super) status_segments: Vec<String>,
    /// Sub-cursor inside the Session → Models row list (row 0 =
    /// Session pin, rows 1..n = each attached agent). Drives which
    /// scope `Enter` opens the picker for. Clamped to live length
    /// each render.
    pub(super) session_models_cursor: usize,
    /// Sub-cursor inside the Peer → MCP list. Clamped to live length
    /// each render; persists across category switches.
    pub(super) peer_mcp_cursor: usize,
    /// Sub-cursor inside the Session → Agents list (`meta.agents`). Same
    /// semantics as `peer_agents_cursor`.
    pub(super) session_agents_cursor: usize,
    /// Bottom-strip inline prompt active in the current Settings page.
    /// `Some` while the user is typing; `None` otherwise. Keys route to
    /// the prompt instead of category navigation when set.
    pub(super) settings_prompt: Option<SettingsPrompt>,
    /// Bottom-strip picker active in the current Settings page. Mutually
    /// exclusive with `settings_prompt` — opening one clears the other.
    /// Keys route to the picker (filter typing + ↑↓ + Enter) when set.
    pub(super) settings_picker: Option<SettingsPicker>,
    /// Peer→Agents yaml↔DB diff/merge modal. `Some` while open; takes over
    /// the Agents detail pane and routes keys to its own handler. Mutually
    /// exclusive with the prompt/picker.
    pub(super) agent_diff: Option<AgentDiffView>,
    /// One-shot status line shown in the Settings status strip in place
    /// of the regular hints. Set by action keys (`[r]` reload, `[d]`
    /// remove, etc.) to confirm what just happened; cleared on the next
    /// navigation keypress.
    pub(super) settings_status: Option<String>,
    /// Mode to restore when the model picker closes (Esc or selection).
    /// Set when the picker opens; used so opening the picker from inside
    /// Session Settings returns there rather than dumping the user back
    /// to Chat. Defaults to Chat — the historical behavior — when the
    /// picker is opened from chat-mode.
    pub(super) model_picker_caller: TuiMode,
    /// Mode to restore when the user hits Esc inside a Settings page.
    /// Set on entry to Settings; cleared on exit. One step deep — Settings
    /// pages don't nest into other modes that would need a real stack.
    pub(super) settings_return: Option<TuiMode>,
    /// Index into `PeerSettingsCategory::ALL` of the active category in
    /// Peer Settings. Persists across enter/exit so the user lands where
    /// they last were.
    pub(super) peer_settings_index: usize,
    /// Index into `SessionSettingsCategory::ALL` of the active category in
    /// Session Settings.
    pub(super) session_settings_index: usize,
    /// Which pane in the active Settings page owns ↑↓ / Enter input.
    /// Reset to `Category` on `open_settings`; flipped by Right / Enter,
    /// Left, or a click that lands inside the corresponding region.
    pub(super) settings_focus: SettingsFocus,
    pub(super) settings_reader: SettingsReader,
    /// Actual visible row slots, excluding the list header and footer.
    pub(super) settings_list_area: Option<ratatui::layout::Rect>,
}

impl App {
    /// An app focused on one open conversation.
    #[cfg(test)]
    fn new(agent_names: HashSet<String>, initial_tab: Tab) -> Self {
        let mut app = Self::hub(agent_names);
        app.push_tab(initial_tab);
        app.mode = TuiMode::Chat;
        app
    }

    /// The launch state: the session hub with no conversation open.
    fn hub(agent_names: HashSet<String>) -> Self {
        Self {
            mode: TuiMode::SessionPicker,
            overlay: None,
            click_regions: Vec::new(),
            input: String::new(),
            cursor: 0,
            completion: None,
            completion_dismissed: false,
            tabs: Vec::new(),
            active_tab: 0,
            agent_names,
            should_quit: false,
            debug_mode: false,
            startup_ready: true,
            expand_all: false,
            session_list: Vec::new(),
            session_list_fresh: false,
            session_indices: HashMap::new(),
            session_fill: None,
            session_generation: 0,
            session_catalog_loading: false,
            session_picker_error: None,
            hub_notice: None,
            drafts: HashMap::new(),
            session_rows_loading: HashSet::new(),
            session_metadata_limit: Arc::new(Semaphore::new(4)),
            picker_index: 0,
            picker_visible_rows: 0..0,
            scroll: ListScroll::default(),
            model_list: Vec::new(),
            session_catalog: None,
            model_picker_index: 0,
            model_picker_filtered: Vec::new(),
            model_picker_scroll: 0,
            model_search: String::new(),
            model_picker_matcher: nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT),
            model_picker_favorites: Vec::new(),
            model_picker_loading: false,
            model_picker_error: None,
            model_picker_scope: ModelPickerScope::Session,
            model_picker_current_pin: None,
            session_settings_snapshot: None,
            settings_return: None,
            peer_settings_index: 0,
            session_settings_index: 0,
            peer_agents_cursor: 0,
            peer_agents_names: Vec::new(),
            peer_defaults: Vec::new(),
            peer_defaults_cursor: 0,
            peer_mcp_servers: Vec::new(),
            status_segments: Vec::new(),
            peer_mcp_cursor: 0,
            session_models_cursor: 0,
            session_agents_cursor: 0,
            settings_prompt: None,
            settings_picker: None,
            agent_diff: None,
            settings_status: None,
            model_picker_caller: TuiMode::Chat,
            settings_focus: SettingsFocus::Category,
            settings_reader: SettingsReader::default(),
            settings_list_area: None,
        }
    }

    /// Enter Settings in `scope`, remembering `from` so Esc returns there.
    /// No-op when already in Settings (avoids clobbering the return-to mode
    /// if `Ctrl+,` is hit twice).
    pub(super) fn open_settings(&mut self, scope: SettingsScope, from: TuiMode) {
        if matches!(self.mode, TuiMode::Settings(_)) {
            return;
        }
        self.settings_reader = SettingsReader::default();
        self.settings_list_area = None;
        self.settings_return = Some(from);
        self.settings_focus = SettingsFocus::Category;
        self.mode = TuiMode::Settings(scope);
    }

    /// Exit Settings, returning to whichever mode opened it (defaulting to
    /// Chat if the return-to slot was somehow empty).
    pub(super) fn close_settings(&mut self) {
        let back = self.settings_return.take().unwrap_or(TuiMode::Chat);
        self.mode = back;
        // Drop any transient Settings sub-state so re-entry starts clean.
        self.agent_diff = None;
        self.settings_reader = SettingsReader::default();
        self.settings_list_area = None;
        self.click_regions.clear();
    }

    /// Refresh before rendering/input, including while a submodal covers the body.
    /// A replacement at the same numeric cursor is still a different reader.
    pub(super) fn sync_settings_reader(&mut self, scope: SettingsScope) {
        let category = self.settings_index(scope);
        let entity = match scope {
            SettingsScope::Peer => match PeerSettingsCategory::ALL.get(category) {
                Some(PeerSettingsCategory::Agents) => self
                    .peer_agents_names
                    .get(
                        self.peer_agents_cursor
                            .min(self.peer_agents_names.len().saturating_sub(1)),
                    )
                    .cloned(),
                Some(PeerSettingsCategory::Mcp) => self
                    .peer_mcp_servers
                    .get(
                        self.peer_mcp_cursor
                            .min(self.peer_mcp_servers.len().saturating_sub(1)),
                    )
                    .map(|e| e.name.clone()),
                _ => None,
            },
            SettingsScope::Session => self.current().map(|tab| tab.session_db_id.clone()),
        };
        let identity = SettingsReaderIdentity {
            scope,
            category,
            entity,
        };
        if self.settings_reader.identity.as_ref() != Some(&identity) {
            self.settings_reader = SettingsReader {
                identity: Some(identity),
                ..Default::default()
            };
        }
    }

    pub(super) fn settings_category_count(&self, scope: SettingsScope) -> usize {
        match scope {
            SettingsScope::Peer => PeerSettingsCategory::ALL.len(),
            SettingsScope::Session => SessionSettingsCategory::ALL.len(),
        }
    }

    pub(super) fn settings_index(&self, scope: SettingsScope) -> usize {
        match scope {
            SettingsScope::Peer => self.peer_settings_index,
            SettingsScope::Session => self.session_settings_index,
        }
    }

    pub(super) fn set_settings_index(&mut self, scope: SettingsScope, idx: usize) {
        let n = self.settings_category_count(scope);
        if n == 0 {
            return;
        }
        let clamped = idx.min(n - 1);
        if self.settings_index(scope) != clamped {
            self.settings_reader = SettingsReader::default();
            self.settings_list_area = None;
        }
        match scope {
            SettingsScope::Peer => self.peer_settings_index = clamped,
            SettingsScope::Session => self.session_settings_index = clamped,
        }
    }

    /// The focused conversation. Only conversation-scoped modes (Chat,
    /// Session Settings, a session-scoped model picker) may call this; the
    /// hub and Peer Settings run with no conversation open.
    pub(super) fn active(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub(super) fn active_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    /// The focused conversation, if any is open.
    pub(super) fn current(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub(super) fn current_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    /// Whether the focused conversation is waiting on a tool approval.
    pub(super) fn has_pending_approval(&self) -> bool {
        self.current()
            .is_some_and(|tab| tab.pending_approval.is_some())
    }

    /// Focus tab `i`, parking the outgoing conversation's unsent draft and
    /// restoring the incoming one's.
    pub(super) fn set_active_tab(&mut self, i: usize) {
        if i >= self.tabs.len() || (i == self.active_tab && self.current().is_some()) {
            return;
        }
        if let Some(id) = self.current().map(|tab| tab.session_db_id.clone()) {
            let draft = (std::mem::take(&mut self.input), self.cursor);
            if draft.0.is_empty() {
                self.drafts.remove(&id);
            } else {
                self.drafts.insert(id, draft);
            }
        }
        self.active_tab = i;
        self.restore_draft();
    }

    fn restore_draft(&mut self) {
        let draft = self
            .current()
            .and_then(|tab| self.drafts.get(&tab.session_db_id).cloned());
        if let Some(id) = self.current().map(|tab| tab.session_db_id.clone()) {
            self.drafts.remove(&id);
        }
        (self.input, self.cursor) = draft.unwrap_or_default();
        self.completion = None;
        self.completion_dismissed = false;
        input::recompute_completion(self);
    }

    /// Open a conversation view and focus it with an empty composer.
    pub(super) fn push_tab(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.set_active_tab(self.tabs.len() - 1);
    }

    /// Close one conversation view. The stored session, its attachments and
    /// any running turn are untouched; a closed view's draft is discarded.
    pub(super) fn close_tab_at(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let closed = self.tabs.remove(i);
        self.drafts.remove(&closed.session_db_id);
        if i == self.active_tab {
            self.input.clear();
            self.cursor = 0;
            self.active_tab = i.min(self.tabs.len().saturating_sub(1));
            self.restore_draft();
        } else if i < self.active_tab {
            self.active_tab -= 1;
        }
    }

    /// Find a tab hosting the given session DB id, if any.
    pub(super) fn tab_index_for(&self, session_db_id: &str) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| t.session_db_id == session_db_id)
    }

    /// Number of selectable rows in the session picker: a virtual "New
    /// session" row at index 0, then one row per known session.
    pub(super) fn picker_len(&self) -> usize {
        self.session_list.len() + 1
    }

    /// Cancel picker work and invalidate any result already queued for the UI.
    pub(super) fn cancel_session_fill(&mut self) {
        if let Some(fill) = self.session_fill.take() {
            fill.cancel.store(true, Ordering::Relaxed);
        }
        self.session_rows_loading.clear();
        self.session_catalog_loading = false;
        self.session_generation = self.session_generation.wrapping_add(1);
    }

    fn requested_session_indices(&mut self) -> Vec<SessionIndex> {
        let requested: Vec<_> = self
            .picker_visible_rows
            .clone()
            .filter_map(|i| self.session_list.get(i))
            .filter(|row| !row.loaded && !self.session_rows_loading.contains(&row.session_db_id))
            .filter_map(|row| self.session_indices.get(&row.session_db_id).cloned())
            .collect();
        self.session_rows_loading
            .extend(requested.iter().map(|index| index.session_db_id.clone()));
        requested
    }

    /// Resolve the highlighted picker row to a dispatch token: the
    /// `"__new__"` sentinel for the top row, otherwise the session's db id.
    pub(super) fn picker_selection(&self) -> String {
        match self.picker_index.checked_sub(1) {
            None => "__new__".to_string(),
            Some(i) => self
                .session_list
                .get(i)
                .map(|s| s.session_db_id.clone())
                .unwrap_or_else(|| "__new__".to_string()),
        }
    }

    /// Seed the picker with YAML-configured "favorites" and the pin
    /// snapshot for the caller-selected scope. Called when the picker
    /// opens; catalog rows arrive asynchronously via
    /// `Action::ModelsFetched`. The scope is locked at open time —
    /// users pick which scope to edit before the picker mounts (via
    /// the Models settings row list), so there's no in-picker scope
    /// cycling.
    pub(super) fn seed_model_picker(
        &mut self,
        backend: &BackendManager,
        current_pin: Option<String>,
        scope: ModelPickerScope,
    ) {
        self.model_picker_favorites = backend.list_known_models_with_info();
        self.model_picker_error = None;
        self.model_search.clear();
        self.model_picker_scroll = 0;

        self.model_picker_current_pin = current_pin;
        self.model_picker_scope = scope;

        self.rebuild_model_list(Vec::new());
    }

    /// Active scope's pin: the model id currently set in the scope
    /// the picker is editing. `None` when no model is pinned in that
    /// scope. Used to render the `(current)` indicator and the
    /// floating-active sort.
    pub(super) fn active_scope_pin(&self) -> Option<&str> {
        self.model_picker_current_pin.as_deref()
    }

    /// Merge favorites with a catalog list. Favorites pinned at top;
    /// catalog entries duplicating a favorite id are dropped (but the
    /// catalog row's pricing/capabilities are folded into the favorite
    /// first so YAML-declared models still show full prices). The active
    /// scope's pin floats to the very top regardless of which list it
    /// came from — so when the user cycles scopes, that scope's pinned
    /// model jumps to row 0.
    pub(super) fn rebuild_model_list(&mut self, catalog: Vec<ModelInfo>) {
        let current = self
            .active_scope_pin()
            .map(str::to_string)
            .or_else(|| self.current().map(|tab| tab.effective_model.clone()))
            .unwrap_or_default();

        let catalog_by_id: std::collections::HashMap<String, &ModelInfo> =
            catalog.iter().map(|m| (m.id.clone(), m)).collect();

        // Enrich each favorite with catalog data for whichever fields the
        // YAML left blank. Catalog pricing/capability data wins on absent
        // fields; YAML keeps precedence where set so user-overrides hold.
        let mut favs: Vec<ModelInfo> = self
            .model_picker_favorites
            .iter()
            .map(|fav| match catalog_by_id.get(&fav.id) {
                None => fav.clone(),
                Some(cat) => ModelInfo {
                    id: fav.id.clone(),
                    price_input: fav.price_input.or(cat.price_input),
                    price_output: fav.price_output.or(cat.price_output),
                    price_cache_read: fav.price_cache_read.or(cat.price_cache_read),
                    input_modalities: if fav.input_modalities.is_empty() {
                        cat.input_modalities.clone()
                    } else {
                        fav.input_modalities.clone()
                    },
                    output_modalities: if fav.output_modalities.is_empty() {
                        cat.output_modalities.clone()
                    } else {
                        fav.output_modalities.clone()
                    },
                    context_window: fav.context_window.or(cat.context_window),
                },
            })
            .collect();
        favs.sort_by(|a, b| a.id.cmp(&b.id));

        let fav_ids: std::collections::HashSet<String> =
            favs.iter().map(|m| m.id.clone()).collect();
        let mut catalog_only: Vec<ModelInfo> = catalog
            .into_iter()
            .filter(|m| !fav_ids.contains(&m.id))
            .collect();
        catalog_only.sort_by(|a, b| a.id.cmp(&b.id));

        let mut out: Vec<ModelInfo> = Vec::new();
        out.extend(favs);
        out.extend(catalog_only);

        // Floating-active sort applied after merge so the current model
        // appears at the top whether it lives in favorites or catalog.
        out.sort_by(|a, b| {
            let a_active = a.id == current;
            let b_active = b.id == current;
            match (a_active, b_active) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        });

        self.model_list = out;
        self.recompute_model_filter();
    }

    /// Recompute `model_picker_filtered` from `model_search` against
    /// `model_list`. Empty query keeps `model_list` order verbatim;
    /// non-empty query keeps only rows the matcher scores positively,
    /// sorted by descending score.
    pub(super) fn recompute_model_filter(&mut self) {
        use nucleo_matcher::Utf32String;
        use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};

        let prev_selected_idx = self
            .model_picker_filtered
            .get(self.model_picker_index)
            .copied();

        if self.model_search.is_empty() {
            self.model_picker_filtered = (0..self.model_list.len()).collect();
        } else {
            let pattern = Pattern::parse(
                &self.model_search,
                CaseMatching::Ignore,
                Normalization::Smart,
            );
            // Trick: parse_into uses `AtomKind::Fuzzy` by default which is
            // exactly what we want — same scoring fzf uses. No retyping
            // needed unless we want exact/prefix modes later.
            let _ = AtomKind::Fuzzy;

            let mut scored: Vec<(usize, u32)> = self
                .model_list
                .iter()
                .enumerate()
                .filter_map(|(i, m)| {
                    let haystack = Utf32String::from(model_searchable(m));
                    pattern
                        .score(haystack.slice(..), &mut self.model_picker_matcher)
                        .map(|score| (i, score))
                })
                .collect();
            // Higher score first; break ties by original list order so the
            // current/favorites pinning stays stable.
            scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            self.model_picker_filtered = scored.into_iter().map(|(i, _)| i).collect();
        }

        // Preserve cursor on the same model where possible, else snap to top.
        self.model_picker_index = prev_selected_idx
            .and_then(|orig| self.model_picker_filtered.iter().position(|&i| i == orig))
            .unwrap_or(0);
        self.model_picker_scroll = 0;
    }

    /// Resolve the currently highlighted picker row to its model id, if any.
    pub(super) fn model_picker_selection(&self) -> Option<String> {
        self.model_picker_filtered
            .get(self.model_picker_index)
            .and_then(|&i| self.model_list.get(i))
            .map(|m| m.id.clone())
    }

    /// Point the picker cursor at `session_db_id` (offset past the New
    /// session row). Falls back to the first session, or the New row when
    /// there are no sessions.
    pub(super) fn focus_picker_on(&mut self, session_db_id: &str) {
        self.picker_index = self
            .session_list
            .iter()
            .position(|s| s.session_db_id == session_db_id)
            .map(|p| p + 1)
            .unwrap_or(if self.session_list.is_empty() { 0 } else { 1 });
    }
}

fn init_terminal() -> anyhow::Result<Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

/// Register a session DB with the server and wire up per-tab notify and
/// approval forwarding. The raw approval channel given to the server is
/// per-session; a spawned forwarder tags each approval with the session_db_id
/// and pushes into the shared TUI approval channel.
async fn setup_session(
    server: &Server,
    session_db: &eidetica::Database,
    backend: BackendManager,
    approval_tx: mpsc::Sender<TaggedApproval>,
    notify_tx: mpsc::Sender<String>,
) -> anyhow::Result<()> {
    let session_db_id = session_db.root_id().to_string();

    server.load_session_entities(session_db).await?;

    if server.is_executor_authorized() {
        // Per-session raw approval channel → tagged forward to shared channel.
        let (raw_tx, mut raw_rx) = mpsc::channel::<ApprovalExchange>(8);
        let forwarder_id = session_db_id.clone();
        let forwarder_tx = approval_tx.clone();
        tokio::spawn(async move {
            while let Some(ex) = raw_rx.recv().await {
                if forwarder_tx.send((forwarder_id.clone(), ex)).await.is_err() {
                    break;
                }
            }
        });
        server
            .register_session(session_db, backend, None, Some(raw_tx))
            .await?;
    } else {
        server
            .watch_session(session_db, backend, None, None)
            .await?;
    }

    let notify_id = session_db_id;
    session_db
        .on_write(move |event, _db| {
            // Source-agnostic: a co-owner's message landing over sync should
            // redraw the tab as readily as one typed here.
            tracing::trace!(session = %notify_id, source = ?event.source(), "Session write; redrawing the tab");
            let tx = notify_tx.clone();
            let id = notify_id.clone();
            Box::pin(async move {
                let _ = tx.send(id).await;
                Ok(())
            })
        })
        .await?
        .detach();

    Ok(())
}

/// `--session NAME`: reopen the session with that name, or create and name
/// one. An existing name that fails to open is an error, never a reason to
/// create a second session or fall back to another conversation.
async fn open_named_session(server: &Server, name: &str) -> anyhow::Result<eidetica::Database> {
    use anyhow::Context as _;
    if let Some(id) = server.registry().find_by_name(name).await? {
        let (_conv_id, db) = server
            .registry()
            .open_session(&id)
            .await
            .with_context(|| format!("Failed to open session '{name}'"))?;
        return Ok(db);
    }
    let (_conv_id, db) = server.registry().create_session(Some("tui")).await?;
    let session_db_id = db.root_id().to_string();
    server
        .registry()
        .set_session_name(&session_db_id, name.to_string())
        .await
        .with_context(|| format!("Failed to name new session '{name}'"))?;
    // Mirror routing reality in `meta.agents`, as `--print --session` does.
    server.auto_attach_default_agent(&session_db_id).await;
    Ok(db)
}

/// Resolve the conversation a launch opens directly: the named session, a new
/// one for a bare prompt, or none for the hub.
async fn launch_session(
    server: &Arc<Server>,
    session: Option<&str>,
    prompt: bool,
) -> anyhow::Result<Option<eidetica::Database>> {
    if let Some(name) = session {
        return open_named_session(server, name).await.map(Some);
    }
    if !prompt {
        return Ok(None);
    }
    match commands::dispatch_without_session(Command::NewSession(None), server).await {
        Ok(CommandOutcome::SessionSwitched(switch)) => Ok(Some(switch.db)),
        Ok(CommandOutcome::Error(e)) => anyhow::bail!(e),
        _ => anyhow::bail!("Failed to create a session for the prompt"),
    }
}

/// Compose the haystack the fuzzy matcher scores against for a given
/// model. The id is the primary key, but we also fold in capability
/// labels (`vision`, `audio`, `video`, `image-gen`, `audio-gen`) so
/// typing `vision` in the picker filters down to vision-capable
/// models without a separate filter UI.
pub(super) fn model_searchable(m: &ModelInfo) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(6);
    parts.push(&m.id);
    if m.input_modalities.iter().any(|s| s == "image") {
        parts.push("vision");
    }
    if m.input_modalities.iter().any(|s| s == "audio") {
        parts.push("audio");
    }
    if m.input_modalities.iter().any(|s| s == "video") {
        parts.push("video");
    }
    if m.output_modalities.iter().any(|s| s == "image") {
        parts.push("image-gen");
    }
    if m.output_modalities.iter().any(|s| s == "audio") {
        parts.push("audio-gen");
    }
    parts.join(" ")
}

/// Compact capability badge string for the picker's Caps column. One
/// uppercase letter per capability (text omitted — it's the baseline):
/// `V` vision, `A` audio in, `M` movie/video, `I` image-gen, `S` speech.
/// Empty when only text/text.
pub(super) fn model_caps_badge(m: &ModelInfo) -> String {
    let mut badge = String::new();
    if m.input_modalities.iter().any(|s| s == "image") {
        badge.push('V');
    }
    if m.input_modalities.iter().any(|s| s == "audio") {
        badge.push('A');
    }
    if m.input_modalities.iter().any(|s| s == "video") {
        badge.push('M');
    }
    if m.output_modalities.iter().any(|s| s == "image") {
        badge.push('I');
    }
    if m.output_modalities.iter().any(|s| s == "audio") {
        badge.push('S');
    }
    badge
}

/// Set the picker into "loading" and spawn a background task to pull the live
/// `/models` catalog; the result arrives on the UI thread as
/// `Action::ModelsFetched`, which caches it in memory for the session. The
/// catalog is intentionally never persisted — only the model you actually
/// switch to or use is (see `model_info_store`).
fn spawn_catalog_load(
    app: &mut App,
    backend: BackendManager,
    models_tx: mpsc::Sender<Result<Vec<ModelInfo>, String>>,
) {
    app.model_picker_loading = true;
    app.model_picker_error = None;
    tokio::spawn(async move {
        let res = backend
            .fetch_models_with_info()
            .await
            .map_err(|e| e.to_string());
        let _ = models_tx.send(res).await;
    });
}

/// Enter the picker synchronously, then fetch catalog metadata in the
/// background. The event loop remains free to render and cancel while the
/// registry transaction is delayed.
fn open_session_picker(
    app: &mut App,
    server: &Arc<Server>,
    session_rows_tx: &mpsc::Sender<SessionLoad>,
) {
    app.cancel_session_fill();
    app.picker_index = 0;
    app.mode = TuiMode::SessionPicker;
    app.session_picker_error = None;
    app.hub_notice = None;

    if app.session_list_fresh {
        return;
    }

    app.session_list.clear();
    app.session_indices.clear();
    app.session_catalog_loading = true;
    let registry = server.registry_arc();
    spawn_session_catalog_load(app, session_rows_tx.clone(), async move {
        registry
            .list_sessions()
            .await
            .map_err(|error| error.to_string())
    });
}

fn spawn_session_catalog_load<F>(app: &mut App, tx: mpsc::Sender<SessionLoad>, load: F)
where
    F: Future<Output = Result<Vec<SessionIndex>, String>> + Send + 'static,
{
    let generation = app.session_generation;
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    tokio::spawn(async move {
        let result = load.await;
        if !worker_cancel.load(Ordering::Relaxed) {
            let _ = tx.send(SessionLoad::Catalog { generation, result }).await;
        }
    });
    app.session_fill = Some(SessionFill { generation, cancel });
}

fn apply_session_catalog(app: &mut App, indices: Vec<SessionIndex>) {
    let mut cached: HashMap<String, SessionInfo> = std::mem::take(&mut app.session_list)
        .into_iter()
        .filter(|row| row.loaded)
        .map(|row| (row.session_db_id.clone(), row))
        .collect();
    app.session_indices = indices
        .iter()
        .map(|index| (index.session_db_id.clone(), index.clone()))
        .collect();
    app.session_list = indices
        .iter()
        .map(|index| {
            cached
                .remove(&index.session_db_id)
                .unwrap_or_else(|| SessionInfo::placeholder(index))
        })
        .collect();
    commands::sort_session_infos(&mut app.session_list);
    app.session_list_fresh = true;
    app.session_catalog_loading = false;
}

fn apply_session_catalog_result(
    app: &mut App,
    generation: u64,
    result: Result<Vec<SessionIndex>, String>,
) -> CatalogLoadOutcome {
    if generation != app.session_generation || !matches!(app.mode, TuiMode::SessionPicker) {
        return CatalogLoadOutcome::Ignored;
    }
    match result {
        Ok(indices) => {
            apply_session_catalog(app, indices);
            CatalogLoadOutcome::Loaded
        }
        Err(error) => {
            app.session_catalog_loading = false;
            app.session_picker_error = Some(error);
            CatalogLoadOutcome::Failed
        }
    }
}

fn apply_session_row(app: &mut App, generation: u64, info: SessionInfo) -> bool {
    if generation != app.session_generation || !matches!(app.mode, TuiMode::SessionPicker) {
        return false;
    }
    app.session_rows_loading.remove(&info.session_db_id);
    if let Some(row) = app
        .session_list
        .iter_mut()
        .find(|row| row.session_db_id == info.session_db_id)
    {
        *row = info;
    }
    true
}

fn request_session_metadata(
    app: &mut App,
    server: &Arc<Server>,
    session_rows_tx: &mpsc::Sender<SessionLoad>,
) {
    let requested = app.requested_session_indices();
    if requested.is_empty() {
        return;
    }
    let registry = server.registry_arc();
    spawn_session_metadata_load(app, requested, session_rows_tx.clone(), move |index| {
        let registry = registry.clone();
        async move { commands::load_session_metadata(&registry, index).await }
    });
}

fn spawn_session_metadata_load<F, Fut>(
    app: &mut App,
    requested: Vec<SessionIndex>,
    tx: mpsc::Sender<SessionLoad>,
    load: F,
) where
    F: Fn(SessionIndex) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = SessionInfo> + Send + 'static,
{
    let generation = app.session_generation;
    let limit = app.session_metadata_limit.clone();
    let cancel = app
        .session_fill
        .as_ref()
        .filter(|fill| fill.generation == generation)
        .map(|fill| fill.cancel.clone())
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let worker_cancel = cancel.clone();
    tokio::spawn(async move {
        let load = Arc::new(load);
        let mut rows = JoinSet::new();
        for index in requested {
            let limit = limit.clone();
            let load = load.clone();
            let worker_cancel = worker_cancel.clone();
            rows.spawn(async move {
                if worker_cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let _permit = limit.acquire_owned().await.ok()?;
                if worker_cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let info = load(index).await;
                (!worker_cancel.load(Ordering::Relaxed)).then_some(info)
            });
        }

        while let Some(Ok(Some(info))) = rows.join_next().await {
            if tx
                .send(SessionLoad::Row { generation, info })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    app.session_fill = Some(SessionFill { generation, cancel });
}

/// Build a `Tab` for an already-registered session DB.
async fn build_tab(
    server: &Server,
    backend: &BackendManager,
    session_db: eidetica::Database,
    session_db_id: String,
) -> Tab {
    let agent = server
        .registry()
        .resolve_agent(&session_db_id, None, server.agent_index())
        .await;
    let session = Session::new(
        chaz_core::types::ConversationId(session_db_id.clone()),
        session_db.clone(),
    )
    .await;
    let meta = session.read_meta().await;
    let roster = build_roster(server, backend, &meta);
    let session_name = meta.name.clone();
    // Reopening a session must honor persisted pins just like a live refresh.
    let requested_model = meta
        .resolve_model_for_agent(&agent.name)
        .or(agent.default_model.as_deref());
    let effective_model = backend.resolve_model_name(requested_model);
    let context_model = requested_model
        .map(str::to_string)
        .or_else(|| backend.default_model())
        .unwrap_or_default();
    let context_budget = server.effective_context_budget(&context_model, agent.max_context_tokens);
    let entries = session.entries().to_vec();
    let active_turns = Session::active_turn_attempts(&session_db)
        .await
        .unwrap_or_default()
        .len();
    Tab {
        session_db_id,
        session_db,
        entries,
        scroll_offset: 0,
        pending_approval: None,
        active_turns,
        current_agent: agent.name.clone(),
        session_name,
        effective_model,
        roster,
        context_budget,
        model_pin: meta
            .resolve_model_for_agent(&agent.name)
            .map(str::to_string),
        expanded_entries: HashSet::new(),
    }
}

/// Refresh live activity independently of historical transcript entries.
async fn refresh_tab_activity(tab: &mut Tab) -> anyhow::Result<()> {
    tab.active_turns = Session::active_turn_attempts(&tab.session_db).await?.len();
    Ok(())
}

/// Shift the active tab by `delta` (wraps around), only in the conversation
/// view. Settings and pickers retain their caller and scoped identity.
fn cycle_tab(app: &mut App, delta: i32) {
    if app.mode != TuiMode::Chat || app.tabs.is_empty() {
        return;
    }
    let n = app.tabs.len() as i32;
    let i = (app.active_tab as i32 + delta).rem_euclid(n);
    app.set_active_tab(i as usize);
}

/// The hub is the base view: whenever no conversation is open, a
/// conversation-scoped mode falls back to it.
fn ensure_view(app: &mut App, server: &Arc<Server>, session_rows_tx: &mpsc::Sender<SessionLoad>) {
    let needs_conversation = match app.mode {
        TuiMode::Chat | TuiMode::Settings(SettingsScope::Session) => true,
        TuiMode::ModelPicker => !matches!(app.model_picker_scope, ModelPickerScope::AgentGlobal(_)),
        TuiMode::SessionPicker | TuiMode::Settings(SettingsScope::Peer) => false,
    };
    if app.tabs.is_empty() && needs_conversation {
        app.settings_return = None;
        app.session_settings_snapshot = None;
        open_session_picker(app, server, session_rows_tx);
    } else if app.mode == TuiMode::SessionPicker
        && !app.session_list_fresh
        && !app.session_catalog_loading
        && app.session_picker_error.is_none()
    {
        // Settings can interrupt the initial catalog load. Resume it when
        // the hub becomes visible, without rendering an incomplete empty list.
        open_session_picker(app, server, session_rows_tx);
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_chat_action(
    action: ChatAction,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
    models_tx: &mpsc::Sender<Result<Vec<ModelInfo>, String>>,
    session_rows_tx: &mpsc::Sender<SessionLoad>,
) {
    match action {
        ChatAction::SendMessage(text) => {
            let tab = app.active_mut();
            let session_db = tab.session_db.clone();
            let session_db_id = tab.session_db_id.clone();
            let mut session =
                Session::new(chaz_core::types::ConversationId(session_db_id), session_db).await;
            // Only wait on a reply the agent can actually see: an unwritten
            // message never wakes a turn, so the spinner would never stop.
            match session
                .add_entry(SessionEntry {
                    sender: "user".to_string(),
                    content: text,
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Message,
                    metadata: None,
                    routing: None,
                })
                .await
            {
                Ok(_) => {}
                Err(e) => tracing::error!("Failed to send message: {e}"),
            }
        }
        ChatAction::OpenSettings(scope) => {
            // From a chat-action context the caller mode is always Chat —
            // the picker doesn't go through ChatAction. `Ctrl+,` from the
            // picker takes a different path that sets the return-to slot
            // correctly.
            if let SettingsScope::Session = scope {
                seed_session_settings_snapshot(app, server).await;
            }
            app.open_settings(scope, TuiMode::Chat);
        }
        ChatAction::OpenModelPicker { scope } => {
            // Read the current pin for the caller-selected scope so we
            // can show `(current)` in the picker. Session and Agent
            // scopes read from session meta; AgentGlobal reads from
            // the in-memory agent registry.
            let current_pin = match &scope {
                ModelPickerScope::Session | ModelPickerScope::Agent(_) => {
                    let session_db = app.active().session_db.clone();
                    let session_db_id = app.active().session_db_id.clone();
                    let session =
                        Session::new(chaz_core::types::ConversationId(session_db_id), session_db)
                            .await;
                    let meta = session.read_meta().await;
                    match &scope {
                        ModelPickerScope::Session => meta.model.clone(),
                        ModelPickerScope::Agent(name) => meta.agent_models.get(name).cloned(),
                        _ => unreachable!(),
                    }
                }
                ModelPickerScope::AgentGlobal(name) => server
                    .agents()
                    .get(name)
                    .and_then(|a| a.default_model.clone()),
            };
            app.seed_model_picker(backend, current_pin, scope);
            // Reuse the session's in-memory catalog if we've already pulled it;
            // otherwise kick off a live fetch. Browsing never persists — only
            // the model the user selects does (see `dispatch_model_selection`).
            match app.session_catalog.clone() {
                Some(catalog) => app.rebuild_model_list(catalog),
                None => spawn_catalog_load(app, backend.clone(), models_tx.clone()),
            }
            // Remember which mode opened the picker so Esc / selection
            // return there instead of dropping back to chat. From chat
            // this no-ops (caller == Chat); from Settings(Session) it
            // bounces back into the page where the user pressed Enter.
            app.model_picker_caller = app.mode;
            app.mode = TuiMode::ModelPicker;
        }
        ChatAction::OpenModelsSettings => {
            // /models lands on the Models page regardless of what was
            // last open in Session Settings, then leaves the user there
            // to pick a row (Session or per-agent) and Enter into the
            // picker.
            seed_session_settings_snapshot(app, server).await;
            let models_idx = SessionSettingsCategory::ALL
                .iter()
                .position(|c| matches!(c, SessionSettingsCategory::Models))
                .unwrap_or(0);
            app.set_settings_index(SettingsScope::Session, models_idx);
            app.session_models_cursor = 0;
            app.open_settings(SettingsScope::Session, TuiMode::Chat);
            app.settings_focus = SettingsFocus::List;
        }
        ChatAction::OpenPicker => {
            open_session_picker(app, server, session_rows_tx);
        }
        ChatAction::Dispatch(cmd) => {
            // Commands that mutate the catalog membership invalidate the
            // picker cache. NameSession / ClearSessionName change a row's
            // name but Action::SessionChanged patches it in place from the
            // session DB's on_write fire, so no wholesale invalidation
            // needed there.
            if matches!(cmd, Command::NewSession(_)) {
                app.session_list_fresh = false;
            }
            let tab = app.active();
            let session_db_id = tab.session_db_id.clone();
            let session_db = tab.session_db.clone();
            let current_agent = tab.current_agent.clone();
            let session_name = tab.session_name.clone();
            let ctx = CommandContext {
                server,
                secrets,
                backend,
                session_db_id: &session_db_id,
                session_db: &session_db,
                current_agent: &current_agent,
                session_name: session_name.as_deref(),
            };
            let outcome = commands::dispatch(cmd, &ctx).await;
            render_outcome(app, outcome, server, backend, approval_tx, notify_tx).await;
        }
    }
}

/// Persist a rename initiated from the session picker's [r] keybinding, then
/// refresh the picker list so the new alias is visible immediately. The
/// rename targets `session_db_id` directly, which may or may not be the
/// active tab — that's why it bypasses the `/name` Command path (which keys
/// off the active session).
async fn apply_picker_rename(
    app: &mut App,
    server: &Arc<Server>,
    session_db_id: String,
    name: Option<String>,
) {
    let result = match &name {
        Some(n) => {
            server
                .registry()
                .set_session_name(&session_db_id, n.clone())
                .await
        }
        None => server.registry().clear_session_name(&session_db_id).await,
    };

    if let Err(e) = result {
        show_error(app, format!("Rename failed: {e}"));
        // Stay in the picker so the user can try again.
        return;
    }

    // Keep the active tab's cached name in sync so its title and status bar
    // update without waiting for a session reopen.
    if let Some(idx) = app.tab_index_for(&session_db_id) {
        app.tabs[idx].session_name = name.clone();
    }

    if let Some(row) = app
        .session_list
        .iter_mut()
        .find(|row| row.session_db_id == session_db_id)
    {
        row.name = name;
    }
    app.focus_picker_on(&session_db_id);
}

/// Open the hub's selected row: focus an already-open view, open a stored
/// session, or create one. Needs no current conversation; a failure stays in
/// the hub with a notice instead of landing in an unrelated conversation.
async fn dispatch_picker_selection(
    selected: String,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
) {
    app.hub_notice = None;
    if let Some(idx) = app.tab_index_for(&selected) {
        app.set_active_tab(idx);
        app.mode = TuiMode::Chat;
        return;
    }
    let cmd = if selected == "__new__" {
        Command::NewSession(None)
    } else {
        Command::SwitchSession(selected)
    };
    // Creating a session from the picker grows the catalog, so the warm
    // cache is now stale — invalidate it (mirrors the `/new` chat path) or
    // the next `/sessions` would show the cached list without this session.
    let invalidates_cache = matches!(cmd, Command::NewSession(_));
    let outcome = match commands::dispatch_without_session(cmd, server).await {
        Ok(outcome) => outcome,
        Err(_) => CommandOutcome::Error("Session command needs an open conversation".into()),
    };
    if invalidates_cache {
        app.session_list_fresh = false;
    }
    if render_outcome(app, outcome, server, backend, approval_tx, notify_tx).await {
        app.mode = TuiMode::Chat;
    }
}

/// Apply the model selected in the model picker. Dispatches either
/// `Command::Model` (Session scope → `SessionMeta.model`) or
/// `Command::AgentModel` (agent scope → `SessionMeta.agent_models`). Both
/// write via `on_write` → `SessionChanged`, which refreshes the
/// status-bar `effective_model` so display moves in step.
#[allow(clippy::too_many_arguments)]
async fn dispatch_model_selection(
    model_id: String,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
) {
    // Snapshot the picked model's info (pricing/window/modalities, as merged
    // from the live catalog) before `model_id` is consumed below, so we can
    // persist it as an in-use model once the selection commits.
    let selected_info = app.model_list.iter().find(|m| m.id == model_id).cloned();
    // Scope was locked when the picker opened; read it back here to
    // pick between the session-wide Command::Model and per-agent
    // Command::AgentModel.
    let scope = app.model_picker_scope.clone();
    let cmd = match scope {
        ModelPickerScope::Session => Command::Model(Some(model_id)),
        ModelPickerScope::Agent(name) => Command::AgentModel {
            agent: name,
            model: Some(model_id),
        },
        ModelPickerScope::AgentGlobal(name) => Command::AgentSet {
            agent_ref: name,
            field: "model".to_string(),
            value: model_id,
        },
    };
    // Agent-global edits come from Peer Settings, which may have no
    // conversation open; only session scopes address the focused one.
    let outcome = match commands::dispatch_without_session(cmd, server).await {
        Ok(outcome) => outcome,
        Err(cmd) => match app.current() {
            Some(tab) => {
                let session_db_id = tab.session_db_id.clone();
                let session_db = tab.session_db.clone();
                let current_agent = tab.current_agent.clone();
                let session_name = tab.session_name.clone();
                let ctx = CommandContext {
                    server,
                    secrets,
                    backend,
                    session_db_id: &session_db_id,
                    session_db: &session_db,
                    current_agent: &current_agent,
                    session_name: session_name.as_deref(),
                };
                commands::dispatch(cmd, &ctx).await
            }
            None => CommandOutcome::Error("No conversation is open".into()),
        },
    };
    // Record the now-in-use model so the runtime budgets its window (this
    // turn's overlay + next startup's warm) without anyone editing YAML.
    if let Some(info) = selected_info {
        server.cache_model_info(&info).await;
    }
    render_outcome(app, outcome, server, backend, approval_tx, notify_tx).await;
    // Return to whoever opened the picker — chat by default; Session
    // Settings when the picker was invoked from there.
    app.mode = app.model_picker_caller;
}

/// Route a `SettingsKey` outcome through the right async backend path.
/// Extracted so the per-mode match arm in `run()` doesn't balloon every
/// time Settings grows a new action verb.
#[allow(clippy::too_many_arguments)]
async fn handle_settings_outcome(
    outcome: input::SettingsKey,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
    models_tx: &mpsc::Sender<Result<Vec<ModelInfo>, String>>,
    session_rows_tx: &mpsc::Sender<SessionLoad>,
) {
    match outcome {
        input::SettingsKey::None => {}
        input::SettingsKey::OpenModelPicker(given_scope) => {
            // When the caller already knows the scope (e.g. Peer→Agents →
            // AgentGlobal), use it directly. Otherwise derive from the
            // cursor (Session→Models path).
            let scope = given_scope.unwrap_or_else(|| models_scope_from_cursor(app));
            handle_chat_action(
                ChatAction::OpenModelPicker { scope },
                app,
                server,
                backend,
                secrets,
                approval_tx,
                notify_tx,
                models_tx,
                session_rows_tx,
            )
            .await;
        }
        input::SettingsKey::DispatchCommand(cmd) => {
            dispatch_settings_command(cmd, app, server, backend, secrets, approval_tx, notify_tx)
                .await;
        }
        input::SettingsKey::PromptSubmit { intent, value } => match intent {
            SettingsPromptIntent::AddSessionAgent => {
                dispatch_settings_command(
                    Command::AgentAdd(value),
                    app,
                    server,
                    backend,
                    secrets,
                    approval_tx,
                    notify_tx,
                )
                .await;
            }
            SettingsPromptIntent::AddPeerDefault => {
                add_peer_default(app, server, value).await;
            }
        },
        input::SettingsKey::OpenAgentDiff { name } => {
            open_agent_diff(app, server, &name).await;
        }
        input::SettingsKey::ApplyAgentMerge { name, mode } => {
            apply_agent_merge(app, server, &name, mode).await;
        }
        input::SettingsKey::WritePeerDefaults(names) => {
            write_peer_defaults(app, server, names).await;
        }
    }
}

/// Validate `name` against the live agent registry, append it to the
/// persisted defaults list, then call through `write_peer_defaults` so
/// runtime + DB stay in lockstep. Duplicate-name nudges land in
/// `settings_status` rather than silently failing.
async fn add_peer_default(app: &mut App, server: &Arc<Server>, name: String) {
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }
    let known: std::collections::HashSet<String> = server.agents().names().into_iter().collect();
    if !known.contains(&name) {
        app.settings_status = Some(format!("No agent named '{name}' — not added"));
        return;
    }
    let mut next = server.default_agents();
    if next.iter().any(|n| n == &name) {
        app.settings_status = Some(format!("'{name}' already in defaults"));
        return;
    }
    next.push(name.clone());
    write_peer_defaults(app, server, next).await;
    app.settings_status = Some(format!("Added '{name}' to defaults"));
}

/// Apply a new `default_agents` list — set it on the running Server so
/// future session creates use it, persist to `chaz_peer` so the
/// override survives restart. Failures land in `settings_status`; the
/// in-memory value still applies even if the persist fails (better
/// debounced UX than silently dropping).
async fn write_peer_defaults(app: &mut App, server: &Arc<Server>, names: Vec<String>) {
    server.set_default_agents(names.clone());
    match server.registry().save_peer_default_agents(&names).await {
        Ok(()) => {
            // settings_status set by the caller for context-specific
            // messages (added, removed, reordered); a generic "Saved"
            // overrides only if nothing more specific applies.
            if app.settings_status.is_none() {
                app.settings_status = Some("Defaults saved".to_string());
            }
        }
        Err(e) => {
            app.settings_status = Some(format!("Saved in-memory; persist failed: {e}"));
        }
    }
}

/// `[r]` on the Peer→Agents list — open the yaml↔DB diff/merge modal for the
/// named agent. Computes the field-level diff server-side and seeds
/// `app.agent_diff`. A `None` diff (no yaml entry, or this peer doesn't host
/// the agent's DB) lands a one-shot status instead of opening an empty modal.
async fn open_agent_diff(app: &mut App, server: &Arc<Server>, name: &str) {
    match server.agent_diff(name).await {
        Ok(Some(diff)) => {
            // Seed pick state to the changed rows so an immediate Enter in
            // pick mode applies the visible drift.
            let picks = diff
                .rows
                .iter()
                .map(|r| r.status != chaz_core::agent_diff::FieldStatus::Unchanged)
                .collect();
            app.agent_diff = Some(AgentDiffView {
                agent_name: name.to_string(),
                diff,
                mode: AgentDiffMode::View,
                cursor: 0,
                picks,
            });
        }
        Ok(None) => {
            app.settings_status = Some(format!(
                "No yaml entry for '{name}', or this peer doesn't host it — nothing to diff"
            ));
        }
        Err(e) => {
            app.settings_status = Some(format!("Diff failed — {e}"));
        }
    }
}

/// Apply a merge mode chosen from inside the diff modal. Delegates to
/// `Server::merge_agent_from_yaml`, which writes the merged DB config and
/// upserts the runtime registry; the edit is live on the agent's next message.
/// Feedback (and the precise "nothing changed" / "not hosted" cases) lands in
/// `app.settings_status`.
async fn apply_agent_merge(
    app: &mut App,
    server: &Arc<Server>,
    name: &str,
    mode: chaz_core::agent_diff::AgentMergeMode,
) {
    match server.merge_agent_from_yaml(name, &mode).await {
        Ok(outcome) if !outcome.found_yaml => {
            app.settings_status = Some(format!("No yaml entry for '{name}' — nothing applied"));
        }
        Ok(outcome) if !outcome.found_db => {
            app.settings_status =
                Some(format!("This peer holds no key for '{name}' — can't merge"));
        }
        Ok(outcome) if !outcome.changed => {
            app.settings_status = Some(format!("{name}: nothing to apply — already in sync"));
        }
        Ok(outcome) => {
            app.settings_status = Some(format!(
                "{name}: merged {} (effective next message)",
                outcome.applied.join(", ")
            ));
        }
        Err(e) => {
            app.settings_status = Some(format!("Merge failed — {e}"));
        }
    }
}

/// Dispatch a backend command initiated from the Settings page. Shared
/// path between `[d]` direct-action keys and submitted prompts. The
/// command's write fires eidetica's `on_write` callback, which posts
/// to `notify_rx` and refreshes the snapshot via `Action::SessionChanged`
/// — same path every other write uses. There's no explicit re-seed
/// here; the next frame may briefly render stale data before the
/// callback lands, matching the picker's behavior.
#[allow(clippy::too_many_arguments)]
async fn dispatch_settings_command(
    cmd: Command,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
) {
    // Settings actions that edit a session exist only in Session scope, but
    // never let a stale action address a view that has since closed.
    let Some(tab) = app.current() else {
        app.settings_status = Some("No conversation is open".into());
        return;
    };
    let session_db_id = tab.session_db_id.clone();
    let session_db = tab.session_db.clone();
    let current_agent = tab.current_agent.clone();
    let session_name = tab.session_name.clone();
    let ctx = CommandContext {
        server,
        secrets,
        backend,
        session_db_id: &session_db_id,
        session_db: &session_db,
        current_agent: &current_agent,
        session_name: session_name.as_deref(),
    };
    let outcome = commands::dispatch(cmd, &ctx).await;
    // Show errors as system messages — they're surfaced next time the
    // user leaves Settings and sees the chat. (A future stage may add a
    // dedicated error strip on the settings page itself.)
    render_outcome(app, outcome, server, backend, approval_tx, notify_tx).await;
}

/// Translate the Models page row cursor into the picker scope.
/// Row 0 = `Session`; rows 1..=n map to `meta.agents[row - 1]`. Falls
/// back to `Session` when the snapshot is missing or the cursor lands
/// past the agent list.
fn models_scope_from_cursor(app: &App) -> ModelPickerScope {
    let Some(snapshot) = app.session_settings_snapshot.as_ref() else {
        return ModelPickerScope::Session;
    };
    if app.session_models_cursor == 0 {
        return ModelPickerScope::Session;
    }
    let agent_idx = app.session_models_cursor - 1;
    match snapshot.agents.get(agent_idx) {
        Some(agent) => ModelPickerScope::Agent(agent.display_name.clone()),
        None => ModelPickerScope::Session,
    }
}

/// Read the active session's meta + index row and stash a frozen snapshot
/// on `App` for the Session Settings page. Called when Session Settings
/// opens and when the active tab fires `SessionChanged` while that page is
/// up. Silent on failure — the snapshot stays whatever it was so the page
/// still renders something coherent.
async fn seed_session_settings_snapshot(app: &mut App, server: &Arc<Server>) {
    let (session_db_id, session_db, entry_count) = {
        let tab = app.active();
        (
            tab.session_db_id.clone(),
            tab.session_db.clone(),
            tab.entries.len(),
        )
    };
    let session = Session::new(
        chaz_core::types::ConversationId(session_db_id.clone()),
        session_db,
    )
    .await;
    let meta = session.read_meta().await;
    let created_at = server
        .registry()
        .list_sessions()
        .await
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|r| r.session_db_id == session_db_id)
                .and_then(|r| r.created_at)
        });

    app.session_settings_snapshot = Some(SessionMetaSnapshot {
        session_db_id,
        model_pin: meta.model.clone(),
        agent_models: meta.agent_models.clone(),
        agents: meta.agents.clone(),
        host_agent_db_id: meta.host_agent_db_id.clone(),
        created_at,
        entry_count,
    });
}

/// Apply a command outcome. Returns whether it focused a conversation view.
async fn render_outcome(
    app: &mut App,
    outcome: CommandOutcome,
    server: &Server,
    backend: &BackendManager,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
) -> bool {
    match outcome {
        CommandOutcome::Text(t) => show_system_msg(app, t),
        CommandOutcome::Error(e) => show_error(app, e),
        CommandOutcome::SessionsList(list) => {
            if list.is_empty() {
                show_system_msg(app, "No sessions found.".to_string());
            } else {
                let mut msg = String::from("Sessions:\n");
                for info in &list {
                    let agent = info.agent_name.as_deref().unwrap_or("default");
                    let name = info
                        .name
                        .as_deref()
                        .map(|n| format!(" \"{n}\""))
                        .unwrap_or_default();
                    let age = info
                        .created_at
                        .map(|t| t.format("%Y-%m-%d").to_string())
                        .unwrap_or_else(|| "—".to_string());
                    msg.push_str(&format!(
                        "\n  {}{} [{}] ({}, {})",
                        info.session_db_id,
                        name,
                        info.bridge.as_str(),
                        agent,
                        age
                    ));
                }
                show_system_msg(app, msg);
            }
        }
        CommandOutcome::SessionSwitched(switch) => {
            let chaz_core::commands::SessionSwitch {
                session_db_id,
                conv_id,
                db,
                agent_name,
                session_name,
            } = *switch;
            // If the session is already open in some tab, switch to it.
            if let Some(idx) = app.tab_index_for(&session_db_id) {
                app.set_active_tab(idx);
                return true;
            }
            if let Err(e) = setup_session(
                server,
                &db,
                backend.clone(),
                approval_tx.clone(),
                notify_tx.clone(),
            )
            .await
            {
                show_error(app, format!("Failed to register session: {e}"));
                return false;
            }
            let session = Session::new(conv_id, db.clone()).await;
            let entries = session.entries().to_vec();
            let active_turns = Session::active_turn_attempts(&db)
                .await
                .unwrap_or_default()
                .len();
            // Mirror `build_tab` — resolve through the backend so the status
            // bar reflects what the runtime would actually use.
            let agent = server.agents().get(&agent_name);
            let agent_default_model = agent.as_ref().and_then(|a| a.default_model.clone());
            let agent_cap = agent.as_ref().and_then(|a| a.max_context_tokens);
            let meta = session.read_meta().await;
            let requested_model = meta
                .resolve_model_for_agent(&agent_name)
                .or(agent_default_model.as_deref());
            let effective_model = backend.resolve_model_name(requested_model);
            let context_model = requested_model
                .map(str::to_string)
                .or_else(|| backend.default_model())
                .unwrap_or_default();
            let context_budget = server.effective_context_budget(&context_model, agent_cap);
            let roster = build_roster(server, backend, &meta);
            app.push_tab(Tab {
                session_db_id,
                session_db: db,
                entries,
                scroll_offset: 0,
                pending_approval: None,
                active_turns,
                model_pin: meta
                    .resolve_model_for_agent(&agent_name)
                    .map(str::to_string),
                current_agent: agent_name,
                session_name,
                effective_model,
                roster,
                context_budget,
                expanded_entries: HashSet::new(),
            });
            return true;
        }
        CommandOutcome::Quit => {
            app.should_quit = true;
        }
    }
    false
}

pub(super) fn show_system_msg(app: &mut App, content: String) {
    show_local(app, content, EntryType::Message);
}

pub(super) fn show_error(app: &mut App, content: String) {
    show_local(app, content, EntryType::Error);
}

/// Show a local result where the user is: in the hub's footer, in Settings'
/// status strip when no conversation is open, else in the focused transcript.
fn show_local(app: &mut App, content: String, entry_type: EntryType) {
    if app.mode == TuiMode::SessionPicker {
        app.hub_notice = Some(content);
        return;
    }
    let Some(tab) = app.current_mut() else {
        app.settings_status = Some(content);
        return;
    };
    tab.entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type,
        metadata: None,
        routing: None,
    });
}

#[cfg(test)]
mod session_picker_tests {
    use super::*;
    use chaz_core::session::{BridgeKind, SessionStatus};
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};

    async fn test_tab() -> Tab {
        let (_instance, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewUser::passwordless("lazy-picker"),
        )
        .await
        .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        Tab {
            session_db_id: db.root_id().to_string(),
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
        }
    }

    #[tokio::test]
    async fn tab_activity_follows_persisted_claim_not_old_ack() {
        // Keep the backend alive: the picker fixture normally drops it because
        // picker tests only inspect tab metadata, not session DB writes.
        let (_instance, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("live-tab"))
                .await
                .unwrap();
        let key = user.get_default_key().unwrap();
        let db = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        let mut tab = test_tab().await;
        tab.session_db_id = db.root_id().to_string();
        tab.session_db = db;
        let mut session = Session::new(
            chaz_core::types::ConversationId(tab.session_db_id.clone()),
            tab.session_db.clone(),
        )
        .await;
        let attempt = session
            .start_turn_attempt(chaz_core::session::TurnRequestId::parse("tui-message"))
            .await
            .unwrap();
        refresh_tab_activity(&mut tab).await.unwrap();
        assert_eq!(tab.active_turns, 1);
        assert!(view::turn_activity_line(tab.active_turns).is_some());
        session.complete_turn_attempt(&attempt, None).await.unwrap();
        session
            .add_entry(SessionEntry {
                sender: "chaz".into(),
                content: String::new(),
                timestamp: chrono::Utc::now(),
                entry_type: EntryType::Ack,
                metadata: None,
                routing: None,
            })
            .await
            .unwrap();
        refresh_tab_activity(&mut tab).await.unwrap();
        assert_eq!(tab.active_turns, 0);
        assert!(view::turn_activity_line(tab.active_turns).is_none());
    }

    #[tokio::test]
    async fn chat_composer_edits_graphemes_and_multiline() {
        let mut app = App::new(HashSet::new(), test_tab().await);
        for c in "a界e\u{301}👍🏽z".chars() {
            input::handle_chat_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            )
            .await;
        }
        assert_eq!(app.cursor, app.input.len());
        input::handle_chat_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)).await;
        input::handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(app.input, "a界e\u{301}z");
        assert_eq!(app.cursor, "a界e\u{301}".len());
        input::handle_chat_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)).await;
        assert_eq!(app.cursor, "a界".len());
        input::handle_chat_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)).await;
        assert_eq!(app.input, "a界\ne\u{301}z");
        assert_eq!(app.cursor, "a界\n".len());
        // Alt+Enter is the break that legacy terminals can actually deliver —
        // bare Shift+Enter reaches us as an unmodified Enter (i.e. send).
        input::handle_chat_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)).await;
        assert_eq!(app.input, "a界\n\ne\u{301}z");
        assert_eq!(app.cursor, "a界\n\n".len());
    }

    /// An unmodified Enter still submits rather than inserting a break: the
    /// draft is taken and the cursor resets.
    #[tokio::test]
    async fn chat_composer_plain_enter_still_submits() {
        let mut app = App::new(HashSet::new(), test_tab().await);
        input::handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        )
        .await;
        input::handle_chat_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    fn index(n: usize) -> SessionIndex {
        SessionIndex {
            session_db_id: format!("session-{n}"),
            source: Some("tui".into()),
            bridge: BridgeKind::Tui,
            created_at: None,
            status: SessionStatus::Active,
            name: None,
        }
    }

    fn draw_picker(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        app.click_regions.clear();
        terminal.draw(|f| view::ui_picker(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    fn visible_ids(app: &App) -> HashSet<String> {
        app.click_regions
            .iter()
            .filter_map(|r| match r.target {
                ClickTarget::PickerSelect(i) => Some(app.session_list[i].session_db_id.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn retained_picker_viewport_loads_visible_metadata_with_new_selected() {
        let mut app = App::new(HashSet::new(), test_tab().await);
        apply_session_catalog(&mut app, (0..100).map(index).collect());
        app.mode = TuiMode::SessionPicker;
        app.scroll.picker = 60;
        // New session is selected, but the retained viewport is nowhere near row 0.
        let before = draw_picker(&mut app, 100, 13);
        assert_eq!(app.scroll.picker, 60);
        assert!(before.iter().any(|r| r.contains("> + New session")));
        let visible = visible_ids(&app);
        assert_eq!(visible.len(), 3, "only rows actually built in the viewport");
        let requested = app.requested_session_indices();
        assert_eq!(
            requested
                .iter()
                .map(|i| i.session_db_id.clone())
                .collect::<HashSet<_>>(),
            visible
        );
        assert!(
            app.requested_session_indices().is_empty(),
            "no duplicate in-flight requests"
        );
        let count = requested.len();
        let (tx, mut rx) = mpsc::channel(8);
        spawn_session_metadata_load(&mut app, requested, tx, |index| async move {
            let mut info = SessionInfo::placeholder(&index);
            info.agent_name = Some("visible-agent".into());
            info.loaded = true;
            info
        });
        for _ in 0..count {
            let SessionLoad::Row { generation, info } =
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("expected metadata row")
            };
            assert!(apply_session_row(&mut app, generation, info));
        }
        let after = draw_picker(&mut app, 100, 13);
        for hit in &app.click_regions {
            if let ClickTarget::PickerSelect(i) = hit.target {
                assert!(after[hit.y as usize].contains("visible-agent"), "{after:?}");
                assert!(app.session_list[i].loaded);
            }
        }
        assert_eq!(
            app.session_list.iter().filter(|row| row.loaded).count(),
            count
        );
        assert!(app.session_list[..60].iter().all(|row| !row.loaded));
        assert!(
            app.requested_session_indices().is_empty(),
            "loaded rows stay cached"
        );
    }

    #[tokio::test]
    async fn picker_metadata_requests_follow_fresh_resize_and_input_frames() {
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = App::new(HashSet::new(), test_tab().await);
        apply_session_catalog(&mut app, (0..100).map(index).collect());
        app.mode = TuiMode::SessionPicker;
        app.scroll.picker = 50;
        assert!(
            app.requested_session_indices().is_empty(),
            "no viewport before first draw"
        );
        draw_picker(&mut app, 100, 12);
        let small = visible_ids(&app);
        let requested = app.requested_session_indices();
        assert_eq!(
            requested
                .iter()
                .map(|i| i.session_db_id.clone())
                .collect::<HashSet<_>>(),
            small
        );
        draw_picker(&mut app, 100, 22);
        let large = visible_ids(&app);
        assert!(large.len() > small.len());
        let extra = app.requested_session_indices();
        assert_eq!(
            extra
                .iter()
                .map(|i| i.session_db_id.clone())
                .collect::<HashSet<_>>(),
            large.difference(&small).cloned().collect()
        );
        // A click on the pinned row retains the viewport, while Down then wheel
        // changes the cursor; the next draw must update requests before dispatch.
        app.picker_index = 53;
        let new = *app
            .click_regions
            .iter()
            .find(|r| matches!(r.target, ClickTarget::PickerNew))
            .unwrap();
        input::handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: new.x,
                row: new.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.picker_index, 0);
        draw_picker(&mut app, 100, 12);
        assert_eq!(visible_ids(&app), small);
        input::handle_picker_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        input::handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 2,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.picker_index, 4);
        draw_picker(&mut app, 100, 12);
        let moved = visible_ids(&app);
        assert!(moved.is_disjoint(&large));
        assert_eq!(
            app.requested_session_indices()
                .iter()
                .map(|i| i.session_db_id.clone())
                .collect::<HashSet<_>>(),
            moved
        );
        app.session_rows_loading.clear();
        draw_picker(&mut app, 100, 7);
        assert_eq!(
            visible_ids(&app).len(),
            1,
            "a clipped row's header is still visible"
        );
        assert_eq!(app.requested_session_indices().len(), 1);
        draw_picker(&mut app, 100, 6);
        assert!(visible_ids(&app).is_empty());
        assert!(
            app.requested_session_indices().is_empty(),
            "no rows fit below pinned header"
        );
    }

    #[tokio::test]
    async fn visible_metadata_requests_are_bounded_and_cancel_invalidates_results() {
        let mut app = App::new(HashSet::new(), test_tab().await);
        let indices: Vec<_> = (0..100).map(index).collect();
        apply_session_catalog(&mut app, indices);
        app.mode = TuiMode::SessionPicker;

        draw_picker(&mut app, 100, 12);
        let first = app.requested_session_indices();
        assert_eq!(first.len(), 3);
        assert!(first.iter().all(|row| row.session_db_id != "session-99"));

        app.picker_index = 100;
        draw_picker(&mut app, 100, 12);
        let scrolled = app.requested_session_indices();
        assert_eq!(scrolled.len(), 3);
        assert!(scrolled.iter().any(|row| row.session_db_id == "session-99"));

        let generation = app.session_generation;
        app.session_fill = Some(SessionFill {
            generation,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        app.cancel_session_fill();
        assert_ne!(app.session_generation, generation);
        assert!(app.session_rows_loading.is_empty());

        let mut stale = SessionInfo::placeholder(&index(0));
        stale.agent_name = Some("stale".into());
        stale.loaded = true;
        assert!(!apply_session_row(&mut app, generation, stale));
        assert!(app.session_list[0].agent_name.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_catalog_keeps_picker_responsive_and_stale_results_are_ignored() {
        let mut app = App::new(HashSet::new(), test_tab().await);
        app.mode = TuiMode::SessionPicker;
        app.session_catalog_loading = true;
        let (tx, mut rx) = mpsc::channel(1);
        spawn_session_catalog_load(&mut app, tx, async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(vec![index(1)])
        });

        assert_eq!(app.picker_index, 0);
        input::handle_picker_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.mode, TuiMode::Chat));
        app.cancel_session_fill();
        assert!(rx.try_recv().is_err());

        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "cancelled load emitted a result");

        app.mode = TuiMode::SessionPicker;
        let current = app.session_generation;
        assert!(matches!(
            apply_session_catalog_result(&mut app, current.wrapping_sub(1), Ok(vec![index(9)])),
            CatalogLoadOutcome::Ignored
        ));
        assert!(app.session_list.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_row_load_is_bounded_and_cancel_stops_further_opens() {
        let mut app = App::new(HashSet::new(), test_tab().await);
        let indices: Vec<_> = (0..100).map(index).collect();
        apply_session_catalog(&mut app, indices);
        app.mode = TuiMode::SessionPicker;
        draw_picker(&mut app, 100, 20);
        let requested = app.requested_session_indices();
        assert!(requested.len() < app.session_list.len());

        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = opens.clone();
        let observed_active = active.clone();
        let observed_max = max_active.clone();
        let (tx, mut rx) = mpsc::channel(32);
        spawn_session_metadata_load(&mut app, requested, tx, move |index| {
            let observed = observed.clone();
            let active = observed_active.clone();
            let max_active = observed_max.clone();
            async move {
                observed.fetch_add(1, Ordering::Relaxed);
                let now_active = active.fetch_add(1, Ordering::Relaxed) + 1;
                max_active.fetch_max(now_active, Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                active.fetch_sub(1, Ordering::Relaxed);
                let mut row = SessionInfo::placeholder(&index);
                row.loaded = true;
                row
            }
        });

        tokio::task::yield_now().await;
        assert_eq!(opens.load(Ordering::Relaxed), 4);
        assert_eq!(max_active.load(Ordering::Relaxed), 4);
        app.cancel_session_fill();
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(opens.load(Ordering::Relaxed), 4);
        assert!(rx.try_recv().is_err());
    }
}
