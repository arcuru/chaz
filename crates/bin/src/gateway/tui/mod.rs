//! Terminal UI gateway. Elm-style architecture:
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
use chaz_core::commands::{self, Command, CommandContext, CommandOutcome, SessionInfo};
use chaz_core::config::Config;
use chaz_core::gateway::{ApprovalExchange, Gateway};
use chaz_core::model_catalog_cache::ModelCatalogCache;
use chaz_core::security::SecretStore;
use chaz_core::server::Server;
use chaz_core::session::{AgentRef, EntryType, Session, SessionEntry, SessionMeta};

use std::collections::HashMap;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

mod input;
mod theme;
mod view;
mod widgets;

/// Name used for the TUI's default/home session. Created on first TUI launch,
/// reopened on subsequent launches.
const TUI_DEFAULT_NAME: &str = "tui";

pub struct TuiGateway {
    config: Config,
    secrets: SecretStore,
    /// Path the running `Config` was loaded from. Stashed so the
    /// Peer→Agents `[r]` reload action can re-parse the yaml without
    /// guessing at the path. `None` allowed for hosts that don't have a
    /// file-backed config (tests, in-process spawns).
    config_path: Option<std::path::PathBuf>,
    /// Optional text to pre-fill the input box with on launch. Mirrors
    /// claude/pi: `chaz "hello"` opens the TUI with "hello" already in the
    /// composer so the user can review and hit Enter. When set, the
    /// session-picker auto-open is skipped — the user clearly wants to
    /// type/send, not navigate.
    initial_prompt: Option<String>,
}

impl TuiGateway {
    pub fn new(config: Config, secrets: SecretStore) -> Self {
        Self {
            config,
            secrets,
            config_path: None,
            initial_prompt: None,
        }
    }

    /// Set the path the loaded `Config` came from so Settings can reload
    /// agent yaml at runtime.
    pub fn with_config_path(mut self, path: std::path::PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    pub fn with_initial_prompt(mut self, prompt: String) -> Self {
        self.initial_prompt = Some(prompt);
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
/// active tab's `SessionMeta`. See `~/brain/ava/proposals/chaz-settings-pages-plan.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsScope {
    Peer,
    Session,
}

/// Which pane in a Settings page currently owns arrow-key input. Sidebar is
/// the default on entry; Right / Enter / a sidebar click sets it. Detail
/// takes over after Right / Enter into a category that owns an inner list
/// (Session→Agents, Peer→Agents, Peer→Defaults) or after a click inside
/// that list. Tab / BackTab cycle categories regardless of focus and snap
/// focus back to Sidebar so the user never gets pinned inside the inner list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsFocus {
    Sidebar,
    Detail,
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
}

impl ModelPickerScope {
    pub(super) fn label(&self) -> &str {
        match self {
            ModelPickerScope::Session => "Session",
            ModelPickerScope::Agent(name) => name,
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
    pub waiting: bool,
    pub current_agent: String,
    pub session_name: Option<String>,
    /// The model the runtime would actually use for this session's next
    /// turn, as resolved by `BackendManager::resolve_model_name` from the
    /// agent's `default_model`. Empty string when no backends are configured.
    /// Resolved at tab construction; if the agent or backend default
    /// changes mid-session the displayed value goes stale until the next
    /// tab open.
    pub effective_model: String,
    /// Full roster of agents attached to this session, each with its
    /// resolved effective model and whether it is the designated host.
    /// Drives the multi-agent status-bar segment; for a single-agent
    /// session the bar falls back to `current_agent`/`effective_model` so
    /// its rendering stays byte-identical. Refreshed wherever
    /// `effective_model` is.
    pub roster: Vec<RosterAgent>,
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
    /// When true, tool calls / tool results / directives render their full
    /// content. When false (default), they collapse to a one-line summary.
    /// Toggled by Ctrl+T or `/expand`.
    pub(super) expand_all: bool,
    pub(super) session_list: Vec<SessionInfo>,
    /// Lazy cache of `session_list`. The cold-list walk
    /// (`Command::ListSessions`) opens every session DB and folds entries —
    /// fine on a session with 7 catalogs, less so at scale. Set to `true`
    /// after a fresh fetch and patched-in-place when a watched session
    /// fires `on_write`; invalidated wholesale when a session is created.
    /// Sessions not in tabs are assumed stable from this process's
    /// perspective; remote sync writes won't invalidate the cache and the
    /// user can re-open the picker to refresh.
    pub(super) session_list_fresh: bool,
    pub(super) picker_index: usize,
    /// Sorted snapshot of the model picker's contents — favorites
    /// (YAML-configured) followed by the live OpenRouter catalog when
    /// available. Repopulated when the picker opens. Sort order: current
    /// effective model first, then favorites alphabetical, then catalog
    /// alphabetical (catalog entries that duplicate a favorite id are
    /// dropped).
    pub(super) model_list: Vec<ModelInfo>,
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
    /// One-shot status line shown in the Settings status strip in place
    /// of the regular hints. Set by action keys (`[r]` reload, `[d]`
    /// remove, etc.) to confirm what just happened; cleared on the next
    /// navigation keypress.
    pub(super) settings_status: Option<String>,
    /// Path the running config was loaded from. Propagated from
    /// `TuiGateway::config_path` into `App` at startup so Stage 4's
    /// yaml reload can re-read the same file the user is editing.
    /// `None` when chaz wasn't booted with a file-backed config.
    pub(super) settings_config_path: Option<std::path::PathBuf>,
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
    /// Reset to `Sidebar` on `open_settings`; flipped by Right / Enter,
    /// Left, or a click that lands inside the corresponding region.
    pub(super) settings_focus: SettingsFocus,
}

impl App {
    fn new(agent_names: HashSet<String>, initial_tab: Tab) -> Self {
        Self {
            mode: TuiMode::Chat,
            overlay: None,
            click_regions: Vec::new(),
            input: String::new(),
            cursor: 0,
            completion: None,
            completion_dismissed: false,
            tabs: vec![initial_tab],
            active_tab: 0,
            agent_names,
            should_quit: false,
            debug_mode: false,
            expand_all: false,
            session_list: Vec::new(),
            session_list_fresh: false,
            picker_index: 0,
            model_list: Vec::new(),
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
            peer_mcp_cursor: 0,
            session_models_cursor: 0,
            session_agents_cursor: 0,
            settings_prompt: None,
            settings_picker: None,
            settings_status: None,
            settings_config_path: None,
            model_picker_caller: TuiMode::Chat,
            settings_focus: SettingsFocus::Sidebar,
        }
    }

    /// Enter Settings in `scope`, remembering `from` so Esc returns there.
    /// No-op when already in Settings (avoids clobbering the return-to mode
    /// if `Ctrl+,` is hit twice).
    pub(super) fn open_settings(&mut self, scope: SettingsScope, from: TuiMode) {
        if matches!(self.mode, TuiMode::Settings(_)) {
            return;
        }
        self.settings_return = Some(from);
        self.settings_focus = SettingsFocus::Sidebar;
        self.mode = TuiMode::Settings(scope);
    }

    /// Exit Settings, returning to whichever mode opened it (defaulting to
    /// Chat if the return-to slot was somehow empty).
    pub(super) fn close_settings(&mut self) {
        let back = self.settings_return.take().unwrap_or(TuiMode::Chat);
        self.mode = back;
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
        match scope {
            SettingsScope::Peer => self.peer_settings_index = clamped,
            SettingsScope::Session => self.session_settings_index = clamped,
        }
    }

    pub(super) fn active(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub(super) fn active_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
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
        meta: &SessionMeta,
        scope: ModelPickerScope,
    ) {
        self.model_picker_favorites = backend.list_known_models_with_info();
        self.model_picker_error = None;
        self.model_search.clear();
        self.model_picker_scroll = 0;

        // Compute the pin for just the active scope. Used for the
        // `(current)` annotation and floating-active sort.
        self.model_picker_current_pin = match &scope {
            ModelPickerScope::Session => meta.model.clone(),
            ModelPickerScope::Agent(name) => meta.agent_models.get(name).cloned(),
        };
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
            .unwrap_or_else(|| self.active().effective_model.clone());

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

    let notify_id = session_db_id;
    session_db
        .on_write(move |_event, _db| {
            let tx = notify_tx.clone();
            let id = notify_id.clone();
            Box::pin(async move {
                let _ = tx.send(id).await;
                Ok(())
            })
        })?
        .detach();

    Ok(())
}

/// Find an existing session named "tui", or create one and name it.
async fn default_tui_session(
    server: &Server,
) -> anyhow::Result<(chaz_core::types::ConversationId, eidetica::Database)> {
    if let Some(id) = server.registry().find_by_name(TUI_DEFAULT_NAME).await? {
        match server.registry().open_session(&id).await {
            Ok(r) => return Ok(r),
            Err(e) => tracing::warn!(id, "Default TUI session unreadable, recreating: {e}"),
        }
    }
    let (conv_id, db) = server.registry().create_session(Some("tui")).await?;
    let session_db_id = db.root_id().to_string();
    if let Err(e) = server
        .registry()
        .set_session_name(&session_db_id, TUI_DEFAULT_NAME.to_string())
        .await
    {
        tracing::warn!("Failed to name default TUI session: {e}");
    }
    // Mirror routing reality in `meta.agents` so `/agents` and the
    // per-agent model picker reflect the agent that will actually answer.
    let _ = server.auto_attach_default_agent(&session_db_id).await;
    Ok((conv_id, db))
}

/// 24-hour cache TTL for the live model catalog. Refresh on demand with
/// the picker's Ctrl+R binding.
const MODEL_CATALOG_TTL: chrono::Duration = chrono::Duration::hours(24);

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

/// Compose the cache key for this `BackendManager` instance. Including the
/// backend names means changing the configured backends gives the picker a
/// fresh cache slot rather than serving the previous config's catalog.
fn model_cache_key(backend: &BackendManager) -> String {
    let mut names: Vec<String> = backend.list_known_backends();
    names.sort();
    if names.is_empty() {
        "backends-v2:".to_string()
    } else {
        format!("backends-v2:{}", names.join(","))
    }
}

/// Set the picker into "loading" and spawn a background task to populate
/// the catalog. The task hits the cache first (unless `force_refresh`),
/// then falls through to a live `/models` fetch; results arrive on the UI
/// thread as `Action::ModelsFetched`.
fn spawn_catalog_load(
    app: &mut App,
    backend: BackendManager,
    cache: ModelCatalogCache,
    cache_key: String,
    models_tx: mpsc::Sender<Result<Vec<ModelInfo>, String>>,
    force_refresh: bool,
) {
    app.model_picker_loading = true;
    app.model_picker_error = None;
    tokio::spawn(async move {
        if !force_refresh
            && let Some(cached) = cache.get(&cache_key).await
            && cached.is_fresh(MODEL_CATALOG_TTL)
        {
            let _ = models_tx.send(Ok(cached.into_models())).await;
            return;
        }
        match backend.fetch_models_with_info().await {
            Ok(models) => {
                if let Err(e) = cache.put(&cache_key, models.clone()).await {
                    tracing::warn!("model catalog cache write failed: {e}");
                }
                let _ = models_tx.send(Ok(models)).await;
            }
            Err(e) => {
                // Fetch failed — fall back to any cached copy, even a stale
                // one, so the picker still shows the OpenRouter catalog the
                // user pulled earlier. Surface the error only when there's
                // nothing to fall back to.
                if let Some(cached) = cache.get(&cache_key).await {
                    let _ = models_tx.send(Ok(cached.into_models())).await;
                } else {
                    let _ = models_tx.send(Err(e.to_string())).await;
                }
            }
        }
    });
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
    let session_name = meta.name;
    // Mirror the runtime's resolution: the live turn passes
    // `agent.default_model` into `runtime::execute`, which calls
    // `BackendManager::resolve_model_name` to strip the backend prefix
    // and fall back to the backend default when None.
    let effective_model = backend.resolve_model_name(agent.default_model.as_deref());
    let entries = session.entries().to_vec();
    Tab {
        session_db_id,
        session_db,
        entries,
        scroll_offset: 0,
        pending_approval: None,
        waiting: false,
        current_agent: agent.name.clone(),
        session_name,
        effective_model,
        roster,
        expanded_entries: HashSet::new(),
    }
}

impl Gateway for TuiGateway {
    async fn run(self, server: Arc<Server>) -> anyhow::Result<()> {
        let (approval_tx, mut approval_rx) = mpsc::channel::<TaggedApproval>(8);
        let (notify_tx, mut notify_rx) = mpsc::channel::<String>(64);
        // One-shot-style delivery of background model catalog fetches.
        // Buffered so a force-refresh kicked off mid-render doesn't block.
        let (models_tx, mut models_rx) = mpsc::channel::<Result<Vec<ModelInfo>, String>>(4);

        let (_conv_id, session_db) = default_tui_session(&server).await?;
        let session_db_id = session_db.root_id().to_string();

        let backend = BackendManager::new(&self.config.backends, self.secrets.clone());
        let catalog_cache = ModelCatalogCache::new(server.registry().chaz_peer().clone());
        let catalog_cache_key = model_cache_key(&backend);

        setup_session(
            &server,
            &session_db,
            backend.clone(),
            approval_tx.clone(),
            notify_tx.clone(),
        )
        .await?;

        let agent_names: HashSet<String> = server
            .agents()
            .names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        let initial_tab = build_tab(&server, &backend, session_db, session_db_id).await;
        let mut app = App::new(agent_names, initial_tab);
        app.settings_config_path = self.config_path.clone();
        if let Some(prompt) = self.initial_prompt.as_ref() {
            app.input = prompt.clone();
            app.cursor = app.input.len();
        }

        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            original_hook(info);
        }));

        let mut terminal = init_terminal()?;
        let mut events = EventStream::new();

        // When prior sessions exist, open straight into the picker so the
        // user picks one (or the New session row) instead of always landing
        // in the default session. A fresh install — only the just-created
        // empty default session — still goes directly to chat. Also
        // skipped when an initial prompt was supplied — the user already
        // signalled "I want to send something now," not "show me sessions."
        if self.initial_prompt.is_none() {
            let (sid, sdb, agent, sname) = {
                let t = app.active();
                (
                    t.session_db_id.clone(),
                    t.session_db.clone(),
                    t.current_agent.clone(),
                    t.session_name.clone(),
                )
            };
            let ctx = CommandContext {
                server: &server,
                secrets: &self.secrets,
                backend: &backend,
                session_db_id: &sid,
                session_db: &sdb,
                current_agent: &agent,
                session_name: sname.as_deref(),
            };
            if let CommandOutcome::SessionsList(list) =
                commands::dispatch(Command::ListSessions, &ctx).await
            {
                let has_known = list.len() > 1 || list.iter().any(|s| s.entry_count > 0);
                if has_known {
                    app.session_list = list;
                    app.session_list_fresh = true;
                    // Always land on the "New session" row when the picker
                    // first opens.
                    app.picker_index = 0;
                    app.mode = TuiMode::SessionPicker;
                }
            }
        }

        loop {
            terminal.draw(|f| view::ui(f, &mut app, &server, &backend, &self.config))?;

            let action = tokio::select! {
                Some(Ok(event)) = events.next() => {
                    match event {
                        Event::Key(key) => Action::Key(key),
                        Event::Mouse(m) => Action::Mouse(m),
                        _ => continue,
                    }
                }
                Some(id) = notify_rx.recv() => Action::SessionChanged(id),
                Some(msg) = approval_rx.recv() => Action::ApprovalRequest(msg),
                Some(res) = models_rx.recv() => Action::ModelsFetched(res),
            };

            match action {
                Action::Key(key) => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.should_quit = true;
                    } else if key.code == KeyCode::Char('d')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.debug_mode = !app.debug_mode;
                    } else if key.code == KeyCode::Char('t')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.expand_all = !app.expand_all;
                    } else if key.code == KeyCode::Char('w')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        close_active_tab(&mut app);
                    } else if key.code == KeyCode::Char(',')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+, opens Settings, picking scope from the
                        // current mode. Chat → Session (routed through
                        // ChatAction so the meta snapshot gets seeded);
                        // picker → Peer (no snapshot needed). Already in
                        // Settings or the model picker? No-op — Esc exits.
                        match app.mode {
                            TuiMode::Chat => {
                                handle_chat_action(
                                    ChatAction::OpenSettings(SettingsScope::Session),
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                    &catalog_cache,
                                    &catalog_cache_key,
                                    &models_tx,
                                )
                                .await;
                            }
                            TuiMode::SessionPicker => {
                                app.open_settings(SettingsScope::Peer, TuiMode::SessionPicker);
                            }
                            TuiMode::ModelPicker | TuiMode::Settings(_) => {}
                        }
                    } else if key.code == KeyCode::Char('p')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // Ctrl+P toggles the session picker. In chat mode it
                        // opens it; in picker mode it dismisses back to chat.
                        match app.mode {
                            TuiMode::Chat => {
                                handle_chat_action(
                                    ChatAction::OpenPicker,
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                    &catalog_cache,
                                    &catalog_cache_key,
                                    &models_tx,
                                )
                                .await;
                            }
                            TuiMode::SessionPicker => {
                                app.mode = TuiMode::Chat;
                            }
                            TuiMode::ModelPicker => {
                                app.mode = app.model_picker_caller;
                            }
                            // Settings users get out via Esc; Ctrl+P is a
                            // no-op here so it doesn't compete with the
                            // category navigation flow.
                            TuiMode::Settings(_) => {}
                        }
                    } else if key.code == KeyCode::PageUp
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cycle_tab(&mut app, -1);
                    } else if key.code == KeyCode::PageDown
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        cycle_tab(&mut app, 1);
                    } else {
                        match input::handle_overlay_key(&mut app, key) {
                            input::OverlayKey::Consumed => continue,
                            input::OverlayKey::RenameSubmit {
                                session_db_id,
                                name,
                            } => {
                                apply_picker_rename(
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    session_db_id,
                                    name,
                                )
                                .await;
                                continue;
                            }
                            input::OverlayKey::NotConsumed => {}
                        }
                        match app.mode {
                            TuiMode::Chat => {
                                if let Some(chat_action) =
                                    input::handle_chat_key(&mut app, key).await
                                {
                                    handle_chat_action(
                                        chat_action,
                                        &mut app,
                                        &server,
                                        &backend,
                                        &self.secrets,
                                        &approval_tx,
                                        &notify_tx,
                                        &catalog_cache,
                                        &catalog_cache_key,
                                        &models_tx,
                                    )
                                    .await;
                                }
                            }
                            TuiMode::SessionPicker => {
                                if let Some(selected) = input::handle_picker_key(&mut app, key) {
                                    dispatch_picker_selection(
                                        selected,
                                        &mut app,
                                        &server,
                                        &backend,
                                        &self.secrets,
                                        &approval_tx,
                                        &notify_tx,
                                    )
                                    .await;
                                }
                            }
                            TuiMode::ModelPicker => {
                                match input::handle_model_picker_key(&mut app, key) {
                                    input::ModelPickerKey::Select(model_id) => {
                                        dispatch_model_selection(
                                            model_id,
                                            &mut app,
                                            &server,
                                            &backend,
                                            &self.secrets,
                                            &approval_tx,
                                            &notify_tx,
                                        )
                                        .await;
                                    }
                                    input::ModelPickerKey::Refresh => {
                                        spawn_catalog_load(
                                            &mut app,
                                            backend.clone(),
                                            catalog_cache.clone(),
                                            catalog_cache_key.clone(),
                                            models_tx.clone(),
                                            true,
                                        );
                                    }
                                    input::ModelPickerKey::None => {}
                                }
                            }
                            TuiMode::Settings(scope) => {
                                let outcome = input::handle_settings_key(&mut app, key, scope);
                                handle_settings_outcome(
                                    outcome,
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                    &catalog_cache,
                                    &catalog_cache_key,
                                    &models_tx,
                                )
                                .await;
                            }
                        }
                    }
                }
                Action::Mouse(m) => {
                    if let Some(outcome) = input::handle_mouse(&mut app, m) {
                        match outcome {
                            input::MouseOutcome::PickerOpenSelected => {
                                let selected = app.picker_selection();
                                dispatch_picker_selection(
                                    selected,
                                    &mut app,
                                    &server,
                                    &backend,
                                    &self.secrets,
                                    &approval_tx,
                                    &notify_tx,
                                )
                                .await;
                            }
                            input::MouseOutcome::TabActivate(i) => {
                                if i < app.tabs.len() {
                                    app.active_tab = i;
                                }
                            }
                            input::MouseOutcome::TabClose(i) => {
                                close_tab_at(&mut app, i);
                            }
                            input::MouseOutcome::ModelPickerOpenSelected => {
                                if let Some(model_id) = app.model_picker_selection() {
                                    dispatch_model_selection(
                                        model_id,
                                        &mut app,
                                        &server,
                                        &backend,
                                        &self.secrets,
                                        &approval_tx,
                                        &notify_tx,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                Action::SessionChanged(id) => {
                    if let Some(idx) = app.tab_index_for(&id) {
                        let (db_id, db) = {
                            let tab = &app.tabs[idx];
                            (tab.session_db_id.clone(), tab.session_db.clone())
                        };
                        let session =
                            Session::new(chaz_core::types::ConversationId(db_id.clone()), db).await;
                        let entries = session.entries().to_vec();
                        let meta = session.read_meta().await;

                        // Decide waiting state from the fresh entries before
                        // moving them into the tab.
                        let clear_waiting = entries.last().is_some_and(|latest| {
                            app.agent_names.contains(&latest.sender)
                                && latest.entry_type == EntryType::Message
                        });

                        // Refresh effective_model from the fresh meta: if
                        // `/model X` or `/model <agent> Y` ran on this
                        // session (or a remote peer pinned a model), the
                        // resolved value moves. Per-agent override beats
                        // the session pin for the tab's current agent.
                        let current_agent = app.tabs.get(idx).map(|t| t.current_agent.clone());
                        let agent_default = current_agent
                            .as_deref()
                            .and_then(|name| server.agents().get(name))
                            .and_then(|a| a.default_model.clone());
                        let session_model = current_agent
                            .as_deref()
                            .and_then(|name| meta.resolve_model_for_agent(name))
                            .map(str::to_string);
                        let effective_model = backend.resolve_model_name(
                            session_model.as_deref().or(agent_default.as_deref()),
                        );
                        // Refresh the full roster too: attach/detach, host
                        // changes, and per-agent model pins all move here.
                        let roster = build_roster(&server, &backend, &meta);

                        let tab = &mut app.tabs[idx];
                        tab.entries = entries;
                        tab.session_name = meta.name.clone();
                        tab.effective_model = effective_model;
                        tab.roster = roster;
                        if clear_waiting {
                            tab.waiting = false;
                        }

                        // If Settings(Session) is up on the same tab,
                        // refresh the snapshot so meta edits (model pin,
                        // agent attach/detach) propagate immediately.
                        if matches!(app.mode, TuiMode::Settings(SettingsScope::Session))
                            && app
                                .session_settings_snapshot
                                .as_ref()
                                .is_some_and(|s| s.session_db_id == db_id)
                        {
                            seed_session_settings_snapshot(&mut app, &server).await;
                        }

                        // Keep the picker cache in lock-step with this tab's
                        // entries so the next picker open doesn't show stale
                        // counts / cost / name.
                        if let Some(row) = app
                            .session_list
                            .iter_mut()
                            .find(|s| s.session_db_id == db_id)
                        {
                            let entries_ref = &app.tabs[idx].entries;
                            row.entry_count = entries_ref.len();
                            row.name = meta.name.clone();
                            row.agent_name = meta.agent_name.clone();
                            row.last_message =
                                chaz_core::session::summarize_last_message(entries_ref);
                            let (cost, reported, calls) =
                                chaz_core::session::sum_session_cost(entries_ref);
                            row.total_cost_usd = cost;
                            row.cost_reported = reported;
                            row.llm_call_count = calls;
                        }
                    }
                }
                Action::ApprovalRequest((id, exchange)) => {
                    if let Some(idx) = app.tab_index_for(&id) {
                        app.tabs[idx].pending_approval = Some(exchange);
                    } else {
                        // Tab was closed but an approval snuck through — deny
                        // so the runtime doesn't hang waiting.
                        let _ = exchange
                            .decision_tx
                            .send(chaz_core::gateway::ApprovalDecision::Deny);
                    }
                }
                Action::ModelsFetched(res) => {
                    app.model_picker_loading = false;
                    match res {
                        Ok(catalog) => {
                            app.model_picker_error = None;
                            app.rebuild_model_list(catalog);
                        }
                        Err(msg) => {
                            app.model_picker_error = Some(msg);
                        }
                    }
                }
            }

            if app.should_quit {
                break;
            }
        }

        restore_terminal();
        Ok(())
    }
}

/// Shift the active tab by `delta` (wraps around).
fn cycle_tab(app: &mut App, delta: i32) {
    if app.tabs.is_empty() {
        return;
    }
    let n = app.tabs.len() as i32;
    let i = (app.active_tab as i32 + delta).rem_euclid(n);
    app.active_tab = i as usize;
}

fn close_active_tab(app: &mut App) {
    close_tab_at(app, app.active_tab);
}

fn close_tab_at(app: &mut App, i: usize) {
    // Refuse to close the last tab — TUI always shows at least one session.
    if app.tabs.len() <= 1 || i >= app.tabs.len() {
        return;
    }
    app.tabs.remove(i);
    if app.active_tab >= app.tabs.len() {
        app.active_tab = app.tabs.len() - 1;
    } else if i < app.active_tab {
        app.active_tab -= 1;
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
    catalog_cache: &ModelCatalogCache,
    catalog_cache_key: &str,
    models_tx: &mpsc::Sender<Result<Vec<ModelInfo>, String>>,
) {
    match action {
        ChatAction::SendMessage(text) => {
            let tab = app.active_mut();
            let session_db = tab.session_db.clone();
            let session_db_id = tab.session_db_id.clone();
            let mut session =
                Session::new(chaz_core::types::ConversationId(session_db_id), session_db).await;
            session
                .add_entry(SessionEntry {
                    sender: "user".to_string(),
                    content: text,
                    timestamp: chrono::Utc::now(),
                    entry_type: EntryType::Message,
                    metadata: None,
                })
                .await;
            tab.waiting = true;
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
            // Read meta synchronously so we can snapshot the pin for the
            // caller-selected scope before the picker mounts.
            let session_db = app.active().session_db.clone();
            let session_db_id = app.active().session_db_id.clone();
            let session =
                Session::new(chaz_core::types::ConversationId(session_db_id), session_db).await;
            let meta = session.read_meta().await;
            app.seed_model_picker(backend, &meta, scope);
            spawn_catalog_load(
                app,
                backend.clone(),
                catalog_cache.clone(),
                catalog_cache_key.to_string(),
                models_tx.clone(),
                false,
            );
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
            app.settings_focus = SettingsFocus::Detail;
        }
        ChatAction::OpenPicker => {
            let tab = app.active();
            let session_db_id = tab.session_db_id.clone();
            // Warm cache short-circuit: skip the full walk when the cached
            // list is still valid. Action::SessionChanged patches in-tab
            // rows in place, and Command::NewSession invalidates wholesale,
            // so a fresh-flagged cache mirrors what a cold fetch would
            // produce. The cost rollup on each row was computed during the
            // prior cold fetch.
            if app.session_list_fresh && !app.session_list.is_empty() {
                app.picker_index = 0;
                app.mode = TuiMode::SessionPicker;
                return;
            }
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
            match commands::dispatch(Command::ListSessions, &ctx).await {
                CommandOutcome::SessionsList(list) => {
                    app.session_list = list;
                    app.session_list_fresh = true;
                    app.picker_index = 0;
                    app.mode = TuiMode::SessionPicker;
                }
                other => {
                    render_outcome(app, other, server, backend, approval_tx, notify_tx).await;
                }
            }
        }
        ChatAction::Dispatch(cmd) => {
            // Commands that mutate the catalog membership invalidate the
            // picker cache. NameSession / ClearSessionName change a row's
            // name but Action::SessionChanged patches it in place from the
            // session DB's on_write fire, so no wholesale invalidation
            // needed there.
            if matches!(cmd, Command::NewSession) {
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
#[allow(clippy::too_many_arguments)]
async fn apply_picker_rename(
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
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

    // Refresh the picker list so the row reflects the new alias and the
    // selection stays anchored on the renamed session.
    let tab = app.active();
    let active_db_id = tab.session_db_id.clone();
    let active_db = tab.session_db.clone();
    let current_agent = tab.current_agent.clone();
    let active_name = tab.session_name.clone();
    let ctx = CommandContext {
        server,
        secrets,
        backend,
        session_db_id: &active_db_id,
        session_db: &active_db,
        current_agent: &current_agent,
        session_name: active_name.as_deref(),
    };
    if let CommandOutcome::SessionsList(list) =
        commands::dispatch(Command::ListSessions, &ctx).await
    {
        app.session_list = list;
        app.session_list_fresh = true;
        app.focus_picker_on(&session_db_id);
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_picker_selection(
    selected: String,
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    secrets: &SecretStore,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
) {
    // If the user picked an already-open session, just activate its tab
    // instead of re-registering it.
    if let Some(idx) = app.tab_index_for(&selected) {
        app.active_tab = idx;
        app.mode = TuiMode::Chat;
        return;
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
    let cmd = if selected == "__new__" {
        Command::NewSession
    } else {
        Command::SwitchSession(selected)
    };
    // Creating a session from the picker grows the catalog, so the warm
    // cache is now stale — invalidate it (mirrors the `/new` chat path) or
    // the next `/sessions` would show the cached list without this session.
    let invalidates_cache = matches!(cmd, Command::NewSession);
    let outcome = commands::dispatch(cmd, &ctx).await;
    if invalidates_cache {
        app.session_list_fresh = false;
    }
    render_outcome(app, outcome, server, backend, approval_tx, notify_tx).await;
    app.mode = TuiMode::Chat;
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
    let tab = app.active();
    let session_db_id = tab.session_db_id.clone();
    let session_db = tab.session_db.clone();
    let current_agent = tab.current_agent.clone();
    let session_name = tab.session_name.clone();
    // Scope was locked when the picker opened; read it back here to
    // pick between the session-wide Command::Model and per-agent
    // Command::AgentModel.
    let scope = app.model_picker_scope.clone();
    let ctx = CommandContext {
        server,
        secrets,
        backend,
        session_db_id: &session_db_id,
        session_db: &session_db,
        current_agent: &current_agent,
        session_name: session_name.as_deref(),
    };
    let cmd = match scope {
        ModelPickerScope::Session => Command::Model(Some(model_id)),
        ModelPickerScope::Agent(name) => Command::AgentModel {
            agent: name,
            model: Some(model_id),
        },
    };
    let outcome = commands::dispatch(cmd, &ctx).await;
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
    catalog_cache: &ModelCatalogCache,
    catalog_cache_key: &str,
    models_tx: &mpsc::Sender<Result<Vec<ModelInfo>, String>>,
) {
    match outcome {
        input::SettingsKey::None => {}
        input::SettingsKey::OpenModelPicker => {
            // Row 0 in the Models page is the session pin; rows 1..n
            // are per-agent overrides indexed against the session
            // settings snapshot. Translate cursor → scope here so the
            // picker mounts pre-scoped.
            let scope = models_scope_from_cursor(app);
            handle_chat_action(
                ChatAction::OpenModelPicker { scope },
                app,
                server,
                backend,
                secrets,
                approval_tx,
                notify_tx,
                catalog_cache,
                catalog_cache_key,
                models_tx,
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
        input::SettingsKey::ReloadPeerAgent { name } => {
            reload_peer_agent_from_yaml(app, server, &name).await;
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

/// Stage 4 — re-read the on-disk chaz yaml, locate the named agent
/// entry, build a fresh runtime `Agent` from it, and upsert into the
/// in-memory `AgentRegistry`. Yaml owns the declarative fields
/// (system_prompt, default_model, allowed_tools, workers, presets,
/// etc.); DB-side state for Living Agents is untouched — the agent's
/// per-agent DB lives in its own subtree and is queried separately.
///
/// Feedback lands in `app.settings_status` so the result shows on the
/// Settings status strip until the user navigates. Failure modes:
/// missing config path, file IO failure, yaml parse error, missing
/// entry. Each gets a distinct status message.
async fn reload_peer_agent_from_yaml(app: &mut App, server: &Arc<Server>, name: &str) {
    let Some(path) = app.settings_config_path.clone() else {
        app.settings_status = Some("Reload unavailable — no config path".to_string());
        return;
    };

    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            app.settings_status = Some(format!("Reload failed — read {}: {e}", path.display()));
            return;
        }
    };
    let config: Config = match serde_yaml::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            app.settings_status = Some(format!("Reload failed — parse: {e}"));
            return;
        }
    };

    match server.agents().build_from_yaml(name, &config) {
        Some(agent) => {
            server.agents().upsert(agent);
            app.settings_status = Some(format!("Reloaded {name} from yaml"));
        }
        None => {
            app.settings_status = Some(format!(
                "No yaml entry for {name} — edit in place coming in Stage 4.5"
            ));
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

async fn render_outcome(
    app: &mut App,
    outcome: CommandOutcome,
    server: &Server,
    backend: &BackendManager,
    approval_tx: &mpsc::Sender<TaggedApproval>,
    notify_tx: &mpsc::Sender<String>,
) {
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
                    let cost = if info.cost_reported {
                        format!(", ${:.4}", info.total_cost_usd)
                    } else {
                        String::new()
                    };
                    msg.push_str(&format!(
                        "\n  {}{} [{}] ({}, {} entries, {}{cost})",
                        info.session_db_id,
                        name,
                        info.gateway.as_str(),
                        agent,
                        info.entry_count,
                        age
                    ));
                    if let Some(preview) = &info.last_message {
                        msg.push_str(&format!("\n    {preview}"));
                    }
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
                app.active_tab = idx;
                return;
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
                return;
            }
            let session = Session::new(conv_id, db.clone()).await;
            let entries = session.entries().to_vec();
            // Mirror `build_tab` — resolve through the backend so the status
            // bar reflects what the runtime would actually use.
            let agent_default_model = server
                .agents()
                .get(&agent_name)
                .and_then(|a| a.default_model.clone());
            let effective_model = backend.resolve_model_name(agent_default_model.as_deref());
            let roster = build_roster(server, backend, &session.read_meta().await);
            app.tabs.push(Tab {
                session_db_id,
                session_db: db,
                entries,
                scroll_offset: 0,
                pending_approval: None,
                waiting: false,
                current_agent: agent_name,
                session_name,
                effective_model,
                roster,
                expanded_entries: HashSet::new(),
            });
            app.active_tab = app.tabs.len() - 1;
        }
        CommandOutcome::Quit => {
            app.should_quit = true;
        }
    }
}

pub(super) fn show_system_msg(app: &mut App, content: String) {
    app.active_mut().entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
    });
}

pub(super) fn show_error(app: &mut App, content: String) {
    app.active_mut().entries.push(SessionEntry {
        sender: "system".to_string(),
        content,
        timestamp: chrono::Utc::now(),
        entry_type: EntryType::Error,
        metadata: None,
    });
}
