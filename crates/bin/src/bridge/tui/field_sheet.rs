//! Declared extension preferences for Peer (agent defaults) and Session
//! (overrides for every agent) Settings. The sheet owns its keys while open;
//! reads and writes run on the operations queue, never on render or input.
use super::operations::Effect;
use chaz_core::extension::{SettingEdit, SettingField, SettingKind, SettingLayer};
use chaz_core::server::Server;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::sync::Arc;

/// The single layer this sheet edits, bound when it was opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    /// Defaults in one exact hosted Agent DB.
    Agent {
        db_id: eidetica::entry::ID,
        name: String,
    },
    /// Overrides for the whole session; `agent_db_id` only previews inheritance.
    Session {
        session_db_id: String,
        agent_db_id: String,
        agent: String,
        incarnation: u64,
    },
}

impl Target {
    pub fn label(&self) -> &str {
        match self {
            Self::Agent { name, .. } | Self::Session { agent: name, .. } => name,
        }
    }

    pub fn still_current(&self, app: &super::App) -> bool {
        match self {
            Self::Agent { .. } => true,
            Self::Session {
                session_db_id,
                incarnation,
                ..
            } => app.current().is_some_and(|tab| {
                &tab.session_db_id == session_db_id && tab.live.incarnation == *incarnation
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Row {
    pub extension: String,
    pub field: &'static SettingField,
    pub value: Value,
    pub source: SettingLayer,
    pub present: bool,
    pub editable: bool,
}

#[derive(Debug)]
pub(super) enum Draft {
    Bool(bool),
    Model { input: String, turn_model: bool },
    Number(String),
}

#[derive(Debug)]
pub(super) enum Mode {
    View,
    Edit(Draft),
    ConfirmReset,
}

pub(super) struct Sheet {
    pub picks: Vec<Target>,
    pub pick_cursor: usize,
    pub target: Option<Target>,
    /// `None` while loading. A failed read is unavailable, not inherited.
    pub rows: Option<Result<Vec<Row>, String>>,
    pub cursor: usize,
    pub mode: Mode,
    pub notice: Option<String>,
    /// Only the accepted operation for this visit may populate its rows.
    pub pending: Option<u64>,
}

pub(super) struct Loaded {
    pub rows: Result<Vec<Row>, String>,
    pub notice: Option<String>,
}

pub(super) enum Update {
    Candidates(Vec<Target>),
    Rows(Loaded),
}

/// Requests for the event loop; it submits them to the operations queue.
pub(super) enum Request {
    SessionAgents {
        id: String,
        incarnation: u64,
    },
    Load(Target),
    Edit {
        target: Target,
        extension: String,
        key: &'static str,
        edit: SettingEdit,
    },
}

impl Sheet {
    pub fn picker(picks: Vec<Target>) -> Self {
        Self {
            picks,
            pick_cursor: 0,
            target: None,
            rows: None,
            cursor: 0,
            mode: Mode::View,
            notice: None,
            pending: None,
        }
    }

    pub fn selected(&self) -> Option<&Row> {
        match &self.rows {
            Some(Ok(rows)) => rows.get(self.cursor),
            _ => None,
        }
    }

    pub fn picking(&self) -> bool {
        self.target.is_none()
    }

    /// Install a completed read, keeping the same field selected.
    pub fn apply(&mut self, loaded: Loaded) {
        let keep = self
            .selected()
            .map(|row| (row.extension.clone(), row.field.key));
        if let (Some((ext, key)), Ok(rows)) = (keep, &loaded.rows) {
            self.cursor = rows
                .iter()
                .position(|r| r.extension == ext && r.field.key == key)
                .unwrap_or(0);
        }
        self.rows = Some(loaded.rows);
        self.notice = loaded.notice;
        self.mode = Mode::View;
        self.pending = None;
    }

    pub fn complete(&mut self, update: Update) {
        match update {
            Update::Candidates(picks) => {
                self.notice = picks
                    .is_empty()
                    .then(|| "No authorized hosted agents in this session".into());
                self.picks = picks;
                self.pending = None;
            }
            Update::Rows(loaded) => self.apply(loaded),
        }
    }
}

/// Do not let persisted values or display labels add terminal control text.
pub(super) fn one_line(text: &str) -> String {
    chaz_core::security::quarantine(text)
        .text
        .replace(['\u{2028}', '\u{2029}'], " ")
}

pub(super) fn display(row: &Row) -> String {
    if row.field.kind.validate(&row.value).is_err() {
        return "(invalid stored value)".into();
    }
    match (&row.value, row.field.kind) {
        (Value::Bool(true), _) => "on".into(),
        (Value::Bool(false), _) => "off".into(),
        (Value::Null, SettingKind::NullableModel) => "(turn model)".into(),
        (Value::String(text), _) => one_line(text),
        (value, _) => value.to_string(),
    }
}

pub(super) fn source_label(row: &Row, target: &Target) -> String {
    match (row.source, target) {
        (SettingLayer::Session, _) => "Session override".into(),
        (SettingLayer::Agent, _) => format!("Agent: {}", one_line(target.label())),
        (SettingLayer::Default, _) => "Extension default".into(),
    }
}

/// Keys while the sheet is open. Returns a request for the event loop.
pub(super) fn handle_key(sheet: &mut Option<Sheet>, key: KeyEvent) -> Option<Request> {
    let s = sheet.as_mut()?;
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return None;
    }
    if s.picking() {
        if s.pending.is_some() && key.code != KeyCode::Esc {
            return None;
        }
        match key.code {
            KeyCode::Up => s.pick_cursor = s.pick_cursor.saturating_sub(1),
            KeyCode::Down => {
                s.pick_cursor = (s.pick_cursor + 1).min(s.picks.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                let pick = s.picks.get(s.pick_cursor)?;
                let label = one_line(pick.label());
                if s.picks
                    .iter()
                    .enumerate()
                    .any(|(i, other)| i != s.pick_cursor && one_line(other.label()) == label)
                {
                    s.notice = Some("Agent label is ambiguous; no target selected".into());
                    return None;
                }
                return Some(Request::Load(pick.clone()));
            }
            KeyCode::Esc => *sheet = None,
            _ => {}
        }
        return None;
    }
    if s.pending.is_some() && key.code != KeyCode::Esc {
        return None;
    }
    match &mut s.mode {
        Mode::View => match key.code {
            KeyCode::Up => s.cursor = s.cursor.saturating_sub(1),
            KeyCode::Down => {
                let len = s
                    .rows
                    .as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .map_or(0, Vec::len);
                s.cursor = (s.cursor + 1).min(len.saturating_sub(1));
            }
            KeyCode::Enter => {
                let row = s.selected().filter(|r| r.editable)?;
                let draft = match row.field.kind {
                    SettingKind::Boolean => Draft::Bool(row.value == Value::Bool(true)),
                    SettingKind::NullableModel => Draft::Model {
                        input: row.value.as_str().unwrap_or_default().to_owned(),
                        turn_model: row.value.is_null(),
                    },
                    SettingKind::Unsigned { .. } => Draft::Number(match &row.value {
                        Value::Number(n) => n.to_string(),
                        _ => String::new(),
                    }),
                };
                s.notice = None;
                s.mode = Mode::Edit(draft);
            }
            KeyCode::Char('r') if s.selected().is_some_and(|r| r.present && r.editable) => {
                s.notice = None;
                s.mode = Mode::ConfirmReset;
            }
            KeyCode::Esc => {
                s.target = None;
                s.rows = None;
                s.notice = None;
                s.pending = None;
            }
            _ => {}
        },
        Mode::ConfirmReset => match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                return request(s, SettingEdit::Reset);
            }
            KeyCode::Char('n') | KeyCode::Esc => s.mode = Mode::View,
            _ => {}
        },
        Mode::Edit(draft) => {
            match (draft, key.code) {
                (_, KeyCode::Esc) => {
                    s.mode = Mode::View;
                    s.notice = None;
                }
                (_, KeyCode::Enter) => {
                    let value = draft_value(&s.mode)?;
                    let kind = s.selected()?.field.kind;
                    if let Err(error) = kind.validate(&value) {
                        // The prompt stays open with the constraint.
                        s.notice = Some(format!("Invalid: {error}"));
                        return None;
                    }
                    return request(s, SettingEdit::Set(value));
                }
                (
                    Draft::Bool(on),
                    KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Char(' '),
                ) => *on = !*on,
                (Draft::Model { turn_model, .. }, KeyCode::Tab) => *turn_model = !*turn_model,
                (Draft::Model { input, turn_model }, KeyCode::Char(c)) => {
                    *turn_model = false;
                    input.push(c);
                }
                (Draft::Model { input, .. }, KeyCode::Backspace) => {
                    input.pop();
                }
                (Draft::Number(input), KeyCode::Char(c)) => input.push(c),
                (Draft::Number(input), KeyCode::Backspace) => {
                    input.pop();
                }
                _ => {}
            }
        }
    }
    None
}

/// Decimal bytes; invalid or overflowing text reaches the same kind checker.
fn draft_value(mode: &Mode) -> Option<Value> {
    let Mode::Edit(draft) = mode else {
        return None;
    };
    Some(match draft {
        Draft::Bool(on) => Value::Bool(*on),
        Draft::Model {
            turn_model: true, ..
        } => Value::Null,
        Draft::Model { input, .. } => Value::String(input.clone()),
        Draft::Number(input) if input.bytes().all(|b| b.is_ascii_digit()) => {
            input.parse::<u64>().map_or(Value::Null, Value::from)
        }
        Draft::Number(_) => Value::Null,
    })
}

fn request(s: &Sheet, edit: SettingEdit) -> Option<Request> {
    let row = s.selected()?;
    Some(Request::Edit {
        target: s.target.clone()?,
        extension: row.extension.clone(),
        key: row.field.key,
        edit,
    })
}

pub(super) fn submit(app: &mut super::App, server: &Arc<Server>, request: Request) {
    let target = match &request {
        Request::SessionAgents { .. } => None,
        Request::Load(target) | Request::Edit { target, .. } => Some(target),
    };
    if target.is_some_and(|target| !target.still_current(app)) {
        if let Some(sheet) = app.field_sheet.as_mut() {
            sheet.apply(Loaded {
                rows: Err("Settings unavailable: the session changed".into()),
                notice: Some("Not saved: the session changed".into()),
            });
        }
        return;
    }
    let (loading, ticket) = match request {
        Request::SessionAgents { id, incarnation } => (
            None,
            super::operations::submit(
                app,
                false,
                None,
                session_agents(server.clone(), id, incarnation),
            ),
        ),
        Request::Load(target) => (
            Some(target.clone()),
            super::operations::submit(app, false, None, load(server.clone(), target)),
        ),
        Request::Edit {
            target,
            extension,
            key,
            edit: change,
        } => (
            None,
            super::operations::submit(
                app,
                true,
                None,
                edit(server.clone(), target, extension, key, change),
            ),
        ),
    };
    if let (Some(ticket), Some(sheet)) = (ticket, app.field_sheet.as_mut()) {
        if let Some(target) = loading {
            sheet.target = Some(target);
            sheet.rows = None;
            sheet.cursor = 0;
        }
        sheet.mode = Mode::View;
        sheet.notice = None;
        sheet.pending = Some(ticket);
    } else if let Some(sheet) = app.field_sheet.as_mut() {
        sheet.notice = app.operations.notice.clone();
    }
}

const UNAVAILABLE: &str = "Settings unavailable; diagnostic text withheld";

async fn session_agents(
    server: Arc<Server>,
    id: String,
    incarnation: u64,
) -> Result<Effect, String> {
    let registry = server.registry();
    let result: anyhow::Result<_> = async {
        let (_, db) = registry.open_session(&id).await?;
        let mut agents = registry
            .authorized_hosted_agents(&db, server.agent_index())
            .await?;
        agents.sort_by(|a, b| (&a.display_name, &a.db_id).cmp(&(&b.display_name, &b.db_id)));
        Ok(agents
            .into_iter()
            .map(|entry| Target::Session {
                session_db_id: id.clone(),
                agent_db_id: entry.db_id.to_string(),
                agent: entry.display_name,
                incarnation,
            })
            .collect())
    }
    .await;
    result
        .map(|picks| Effect::Fields(Box::new(Update::Candidates(picks))))
        .map_err(|error| {
            tracing::warn!(%error, "Extension settings agent list failed");
            UNAVAILABLE.to_owned()
        })
}

/// Read the declared rows for `target`. Session views resolve the agent
/// through checked authorized membership, not the roster label.
async fn open(
    server: &Server,
    target: &Target,
) -> anyhow::Result<(Option<eidetica::Database>, chaz_core::agent_db::AgentDb)> {
    let registry = server.registry();
    let (session, agent_db_id) = match target {
        Target::Agent { db_id, .. } => (None, db_id.clone()),
        Target::Session {
            session_db_id,
            agent_db_id,
            ..
        } => {
            let (_, db) = registry.open_session(session_db_id).await?;
            (Some(db), eidetica::entry::ID::parse(agent_db_id)?)
        }
    };
    let entry = match &session {
        Some(db) => registry
            .authorized_hosted_agents(db, server.agent_index())
            .await?
            .into_iter()
            .find(|entry| entry.db_id == agent_db_id),
        None => server.agent_index().find_by_id(&agent_db_id),
    }
    .ok_or_else(|| anyhow::anyhow!("agent is not an authorized hosted agent"))?;
    let agent = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no key for agent"))?;
    Ok((session, agent))
}

async fn rows(server: &Server, target: &Target) -> anyhow::Result<Vec<Row>> {
    let (session, agent) = open(server, target).await?;
    let layer = if session.is_some() {
        SettingLayer::Session
    } else {
        SettingLayer::Agent
    };
    let hub = server.extensions();
    let mut names = hub.extension_names();
    names.sort_unstable();
    names.dedup();
    let mut out = Vec::new();
    for name in names {
        for row in hub
            .setting_rows(name, layer, session.as_ref(), Some(agent.database()))
            .await?
        {
            out.push(Row {
                extension: name.to_owned(),
                field: row.field,
                value: row.value,
                source: row.source,
                present: row.present_at_target,
                editable: row.editable,
            });
        }
    }
    Ok(out)
}

async fn load(server: Arc<Server>, target: Target) -> Result<Effect, String> {
    let rows = rows(&server, &target).await.map_err(|error| {
        tracing::warn!(%error, "Extension settings read failed");
        UNAVAILABLE.to_owned()
    });
    Ok(Effect::Fields(Box::new(Update::Rows(Loaded {
        rows,
        notice: None,
    }))))
}

async fn edit(
    server: Arc<Server>,
    target: Target,
    extension: String,
    key: &'static str,
    edit: SettingEdit,
) -> Result<Effect, String> {
    let reset = matches!(edit, SettingEdit::Reset);
    let hub = server.extensions();
    let result = async {
        let (session, agent) = open(&server, &target).await?;
        match &target {
            Target::Agent { db_id, .. } => {
                hub.edit_agent_setting(&db_id.to_string(), agent.database(), &extension, key, edit)
                    .await
            }
            Target::Session { session_db_id, .. } => {
                hub.edit_session_setting(
                    session_db_id,
                    &session.expect("session target"),
                    &extension,
                    key,
                    edit,
                )
                .await
            }
        }
    }
    .await;
    let notice = match &result {
        Ok(()) if reset => Some("Removed".into()),
        Ok(()) => Some("Saved".into()),
        Err(error) => {
            tracing::warn!(%error, extension, key, "Extension setting write not confirmed");
            Some("Save not confirmed; check the current value".into())
        }
    };
    let rows = rows(&server, &target).await.map_err(|error| {
        tracing::warn!(%error, "Extension settings read failed");
        UNAVAILABLE.to_owned()
    });
    Ok(Effect::Fields(Box::new(Update::Rows(Loaded {
        rows,
        notice,
    }))))
}
