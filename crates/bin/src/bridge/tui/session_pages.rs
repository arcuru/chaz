//! Selected-session inspectors: bounded entry-time reads, never render-time I/O.
use super::{App, SessionSettingsCategory as Category, SettingsScope, TuiMode, supervised};
use chaz_core::{
    backends::BackendManager,
    security::approval_display::quarantine,
    server::Server,
    session::{AgentRef, EntryType, SessionMeta},
};
use eidetica::{Database, auth::crypto::PublicKey, store::DocStore};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Failure {
    Unavailable,
    Denied,
    Invalid,
    Failed,
    Timeout,
}
impl Failure {
    fn text(self) -> &'static str {
        match self {
            Self::Unavailable => "Unavailable",
            Self::Denied => "Read denied",
            Self::Invalid => "Invalid stored metadata",
            Self::Failed => "Failed to read source; diagnostic text withheld",
            Self::Timeout => "Timed out reading local snapshot",
        }
    }
}
fn failure(e: &eidetica::Error) -> Failure {
    if e.is_permission_denied() {
        Failure::Denied
    } else {
        Failure::Failed
    }
}
fn safe(s: &str) -> String {
    let q = quarantine(s);
    format!("{}{}", q.text, q.note())
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
fn identifier(raw: &str) -> String {
    eidetica::entry::ID::parse(raw)
        .ok()
        .filter(|id| !id.is_empty())
        .map(|id| id.to_string())
        .unwrap_or_else(|| "Invalid identifier metadata".into())
}

pub(super) struct Snapshot {
    pub category: Category,
    pub id: String,
    pub incarnation: u64,
    pub result: Option<Result<Vec<String>, Failure>>,
    pub observed_at: Option<String>,
}
#[derive(Default)]
pub(super) struct Pages {
    server: Option<Arc<Server>>,
    selected: Option<(Category, String, u64)>,
    generation: u64,
    dirty: bool,
    history_rev: u64,
    task: Option<supervised::Task<()>>,
    pub snapshot: Option<Snapshot>,
}
pub(super) struct Loaded {
    generation: u64,
    category: Category,
    id: String,
    incarnation: u64,
    result: Result<Vec<String>, Failure>,
}
fn selected(app: &App) -> Option<(Category, String, u64)> {
    if app.mode != TuiMode::Settings(SettingsScope::Session) {
        return None;
    }
    let category = *Category::ALL.get(app.session_settings_index)?;
    if !matches!(
        category,
        Category::Routing | Category::History | Category::Sharing
    ) {
        return None;
    }
    let tab = app.current()?;
    Some((category, tab.session_db_id.clone(), tab.live.incarnation))
}
/// Existing session notifications coalesce to one follow-up; no polling.
pub(super) fn changed(app: &mut App, id: &str) {
    if selected(app).is_some_and(|(_, selected, _)| selected == id) {
        app.session_pages.dirty = true;
    }
}
pub(super) fn update(
    app: &mut App,
    server: &Arc<Server>,
    backend: &BackendManager,
    tx: &mpsc::Sender<Loaded>,
) {
    let selection = selected(app);
    let replaced = app
        .session_pages
        .server
        .as_ref()
        .is_none_or(|old| !Arc::ptr_eq(old, server));
    if app.session_pages.selected != selection || (selection.is_some() && replaced) {
        let p = &mut app.session_pages;
        p.task = None;
        p.generation = p.generation.wrapping_add(1);
        p.snapshot = None;
        p.dirty = true;
        p.selected = selection.clone();
        p.server = selection.as_ref().map(|_| server.clone());
    }
    let Some((category, id, incarnation)) = selection else {
        return;
    };
    let rev = app.current().unwrap().live.transcript_rev;
    if category == Category::History && rev != app.session_pages.history_rev {
        app.session_pages.dirty = true;
    }
    if !app.session_pages.dirty || app.session_pages.task.is_some() {
        return;
    }
    let tab = app.current().unwrap();
    let db = tab.session_db.clone();
    let valid = db.root_id().to_string() == id;
    if category == Category::History || !valid {
        let result = if !valid {
            Err(Failure::Unavailable)
        } else {
            Ok(history(tab))
        };
        let p = &mut app.session_pages;
        p.dirty = false;
        p.history_rev = rev;
        p.snapshot = Some(Snapshot {
            category,
            id,
            incarnation,
            result: Some(result),
            observed_at: valid.then(now),
        });
        return;
    }
    // Memory-only configuration capture. No backend catalog/model requests.
    let agents = server.agents_arc();
    let index = server.agent_index().clone();
    let registry = server.registry_arc();
    let cached = if tab.current_agent.is_empty() {
        String::new()
    } else {
        safe(&tab.current_agent)
    };
    let backend = backend.clone();
    let generation = app.session_pages.generation;
    let tx = tx.clone();
    let p = &mut app.session_pages;
    p.dirty = false;
    p.snapshot = Some(Snapshot {
        category,
        id: id.clone(),
        incarnation,
        result: None,
        observed_at: None,
    });
    p.task = Some(supervised::Task(tokio::spawn(async move {
        let work = supervised::work(async move {
            let lines = match category {
                Category::Routing => {
                    routing(&db, &registry, &index, &agents, &cached, &backend).await
                }
                Category::Sharing => sharing(&db, &registry).await,
                _ => unreachable!(),
            };
            Ok(lines)
        });
        let result = match tokio::time::timeout(Duration::from_secs(5), work).await {
            Ok(Ok(lines)) => Ok(lines),
            Ok(Err(_)) => Err(Failure::Failed),
            Err(_) => Err(Failure::Timeout),
        };
        let _ = tx
            .send(Loaded {
                generation,
                category,
                id,
                incarnation,
                result,
            })
            .await;
    })));
}
pub(super) fn apply(app: &mut App, loaded: Loaded) {
    if selected(app) != Some((loaded.category, loaded.id.clone(), loaded.incarnation))
        || loaded.generation != app.session_pages.generation
    {
        return;
    }
    app.session_pages.task = None;
    app.session_pages.snapshot = Some(Snapshot {
        category: loaded.category,
        id: loaded.id,
        incarnation: loaded.incarnation,
        observed_at: None,
        result: Some(loaded.result),
    });
}

fn history(tab: &super::Tab) -> Vec<String> {
    let mut counts = BTreeMap::new();
    for name in [
        "Message",
        "BridgeEvent",
        "MatrixObserved",
        "MatrixSend",
        "MatrixSent",
        "Directive",
        "ToolCall",
        "ToolResult",
        "Ack",
        "Error",
        "Summary",
        "ApprovalRequest",
        "ApprovalDecision",
        "Cancel",
        "TurnContext",
        "Unknown",
    ] {
        counts.insert(name, 0usize);
    }
    let mut first = None;
    let mut last = None;
    for entry in &tab.entries {
        let name = match &entry.entry_type {
            EntryType::Message => "Message",
            EntryType::BridgeEvent => "BridgeEvent",
            EntryType::MatrixObserved => "MatrixObserved",
            EntryType::MatrixSend => "MatrixSend",
            EntryType::MatrixSent => "MatrixSent",
            EntryType::Directive => "Directive",
            EntryType::ToolCall => "ToolCall",
            EntryType::ToolResult => "ToolResult",
            EntryType::Ack => "Ack",
            EntryType::Error => "Error",
            EntryType::Summary => "Summary",
            EntryType::ApprovalRequest => "ApprovalRequest",
            EntryType::ApprovalDecision => "ApprovalDecision",
            EntryType::Cancel => "Cancel",
            EntryType::TurnContext => "TurnContext",
            EntryType::Unknown { .. } => "Unknown",
        };
        *counts.get_mut(name).unwrap() += 1;
        first = Some(
            first.map_or(entry.timestamp, |t: chrono::DateTime<chrono::Utc>| {
                t.min(entry.timestamp)
            }),
        );
        last = Some(
            last.map_or(entry.timestamp, |t: chrono::DateTime<chrono::Utc>| {
                t.max(entry.timestamp)
            }),
        );
    }
    let mut out = vec![format!(
        "Cached entries: {} (not DB total)",
        tab.entries.len()
    )];
    if tab.entries.is_empty() {
        out.push("No cached entries; persisted history not verified".into());
    }
    out.extend(
        counts
            .into_iter()
            .map(|(name, count)| format!("Cached {name}: {count}")),
    );
    for (label, time) in [("Earliest", first), ("Latest", last)] {
        out.push(format!(
            "{label} cached time: {}",
            time.map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "Not available: no cached timestamps".into())
        ));
    }
    out
}

pub(super) fn lines(app: &App, category: Category) -> Vec<String> {
    let mut out = vec![
        format!("{} — read-only", category.label()),
        match category {
            Category::Routing => "Scope: selected session + local registry inputs",
            Category::History => "Scope: selected TUI tab cached-entry metadata",
            Category::Sharing => "Scope: selected DB, connected account local view",
            _ => unreachable!(),
        }
        .into(),
    ];
    let id = app.current().map(|t| t.session_db_id.as_str());
    out.push(format!(
        "Session DB: {}",
        id.map(identifier)
            .unwrap_or_else(|| "Unavailable: no selected session".into())
    ));
    if let Some(s) = app.session_pages.snapshot.as_ref().filter(|s| {
        s.category == category
            && Some(s.id.as_str()) == id
            && app
                .current()
                .is_some_and(|t| t.live.incarnation == s.incarnation)
    }) {
        match &s.result {
            None => out.push("Loading local snapshot…".into()),
            Some(Err(e)) => out.push(e.text().into()),
            Some(Ok(rows)) => {
                if let Some(at) = &s.observed_at {
                    out.push(format!(
                        "Freshness: {} sampled at {at}",
                        if category == Category::History {
                            "TUI cache"
                        } else {
                            "local query snapshot"
                        }
                    ));
                }
                out.extend(rows.iter().cloned());
            }
        }
    } else {
        out.push("Page snapshot unavailable".into());
    }
    out.extend(match category {
        Category::Routing => vec![
            "Human-turn precedence: explicit override, first matching authorized mention, authorized recorded host, first authorized candidate, legacy agent_name, default agent.",
            "Mentions/overrides and a separate home gate affect actual turns. No remote execution health is inspected.",
            "Esc back; /agents or /agent room inspects roster/host; /model inspects pins.",
            "/agent host without an argument clears the host; it is not inspection.",
        ],
        Category::History => vec![
            "Cache only; persistence/completeness and compaction/context boundary are not verified.",
            "Esc back; /info inspects counts; /print displays Message/Directive/Summary/Error; /interrupted inspects interrupted turns.",
            "These commands perform their own reads, not proof this cache is complete. No pruning or clearing here.",
        ],
        Category::Sharing => vec![
            "Not host-aggregate sync or network health. Delegated authority is not inspected.",
            "No ticket, grant, revoke or approval UI.",
            "Esc back; /sharing status inspects this account's shared DBs; /sharing requests inspects the source peer's request queue (not current-session-only).",
            "/share generates a ticket and enables serving; /unshare changes local serving, not existing peers' historical access.",
        ],
        _ => unreachable!(),
    }.into_iter().map(String::from));
    out.push("Leave/reopen this page to retry; check the existing command's result".into());
    out
}

/// Registration must be checked: a native viewer alone accepts missing stores.
async fn metadata(db: &Database) -> Result<eidetica::crdt::Doc, Failure> {
    // Registry is not a Store/viewer in this API. Read its exact native
    // registration through the underlying DocStore; never initialize an index.
    let index = db
        .get_store_viewer::<DocStore>("_index")
        .await
        .map_err(|e| failure(&e))?;
    let entry = index.get("meta").await.map_err(|e| {
        if e.is_not_found() {
            Failure::Unavailable
        } else {
            failure(&e)
        }
    })?;
    let kind = entry
        .as_doc()
        .and_then(|d| d.get("type"))
        .and_then(|v| v.as_text());
    if kind != Some("docstore:v0") {
        return Err(Failure::Invalid);
    }
    db.get_store_viewer::<DocStore>("meta")
        .await
        .map_err(|e| failure(&e))?
        .get_all()
        .await
        .map_err(|e| failure(&e))
}
fn text(doc: &eidetica::crdt::Doc, key: &str) -> Result<Option<String>, Failure> {
    doc.get(key)
        .map(|v| v.as_text().map(str::to_owned).ok_or(Failure::Invalid))
        .transpose()
}
fn json<T: serde::de::DeserializeOwned + Default>(
    doc: &eidetica::crdt::Doc,
    key: &str,
) -> Result<T, Failure> {
    text(doc, key)?
        .map(|s| serde_json::from_str(&s).map_err(|_| Failure::Invalid))
        .unwrap_or_else(|| Ok(T::default()))
}
fn field(
    out: &mut Vec<String>,
    label: &str,
    value: &Result<Option<String>, Failure>,
    absent: &str,
) {
    out.push(format!(
        "{label}: {}",
        match value {
            Ok(Some(v)) => safe(v),
            Ok(None) => absent.into(),
            Err(e) => e.text().into(),
        }
    ));
}
async fn routing(
    db: &Database,
    registry: &chaz_core::session::SessionRegistry,
    index: &chaz_core::hosted_index::HostedIndex,
    agents: &chaz_core::agent::AgentRegistry,
    cached: &str,
    backend: &BackendManager,
) -> Vec<String> {
    let mut out = Vec::new();
    match metadata(db).await {
        Err(e) => out.push(format!("Routing metadata: {}", e.text())),
        Ok(doc) => {
            let host = text(&doc, "host_agent_db_id");
            let roster = json::<Vec<AgentRef>>(&doc, "agents");
            let model = text(&doc, "model");
            let overrides = json::<std::collections::HashMap<String, String>>(&doc, "agent_models");
            let resolution = match (&model, &overrides) {
                (Ok(model), Ok(overrides)) => Some(SessionMeta {
                    model: model.clone(),
                    agent_models: overrides.clone(),
                    ..Default::default()
                }),
                _ => None,
            };
            out.push(format!(
                "Freshness: metadata local query snapshot at {}",
                now()
            ));
            out.push(format!(
                "Recorded host DB: {}",
                match &host {
                    Ok(Some(id)) => identifier(id),
                    Ok(None) => "Host not set".into(),
                    Err(e) => e.text().into(),
                }
            ));
            field(&mut out, "Session pin", &model, "Not set");
            match roster {
                Err(e) => out.push(format!("Attached roster metadata: {}", e.text())),
                Ok(roster) => {
                    if roster.is_empty() {
                        out.push("No roster metadata recorded".into());
                    }
                    for a in roster {
                        out.push(format!(
                            "Attached roster: {} / {}",
                            safe(&a.display_name),
                            identifier(&a.db_id)
                        ));
                        out.push(match &a.home_pubkey {
                            None => "Legacy/unset home".into(),
                            Some(raw) => match PublicKey::from_prefixed_string(raw) {
                                Err(_) => "Invalid home public-key metadata".into(),
                                Ok(home) => {
                                    let local = eidetica::entry::ID::parse(&a.db_id).ok().and_then(|id| index.find_by_id(&id));
                                    format!("Recorded home: {}; {}", home.to_prefixed_string(), match local { None => "Local key comparison unavailable", Some(local) if local.pubkey == home => "Matches local agent key; not running-status proof", Some(_) => "Different from local agent key; not running-status proof" })
                                }
                            }
                        });
                        let pin = overrides
                            .as_ref()
                            .map(|m| m.get(&a.display_name).cloned())
                            .map_err(|e| *e);
                        field(&mut out, "Per-agent session override", &pin, "Not set");
                        if let Some(agent) = agents.get(&a.display_name) {
                            field(
                                &mut out,
                                "Local agent default",
                                &Ok(agent.default_model.clone()),
                                "Not set",
                            );
                            match &resolution {
                                Some(meta) => {
                                    let input = meta
                                        .resolve_model_for_agent(&a.display_name)
                                        .or(agent.default_model.as_deref());
                                    field(
                                        &mut out,
                                        "Resolution input",
                                        &Ok(input.map(str::to_owned)),
                                        "Backend default (no pin)",
                                    );
                                    let resolved = backend.resolve_model_name(input);
                                    out.push(format!(
                                        "Local resolved model: {}",
                                        if resolved.is_empty() {
                                            "Model resolution unavailable".into()
                                        } else {
                                            safe(&resolved)
                                        }
                                    ));
                                }
                                _ => out.push(
                                    "Local model resolution unavailable: invalid pin metadata"
                                        .into(),
                                ),
                            }
                        } else {
                            out.push(
                                "Local agent default unavailable: no local Agent registry row"
                                    .into(),
                            );
                            out.push(
                                "Local model resolution unavailable: no local Agent registry row"
                                    .into(),
                            );
                        }
                    }
                }
            }
        }
    }
    match registry.authorized_hosted_agents(db, index).await {
        Ok(candidates) => {
            out.push(format!(
                "Freshness: candidate local query snapshot at {}",
                now()
            ));
            if candidates.is_empty() {
                out.push("No locally indexed Active+Write candidates".into());
            } else {
                out.push(
                    "Locally indexed Active+Write candidates (not total membership or turn order):"
                        .into(),
                );
                out.extend(
                    candidates
                        .iter()
                        .map(|a| format!("{} / {}", safe(&a.display_name), a.db_id)),
                );
            }
        }
        Err(e) => out.push(format!(
            "Candidate query: {}",
            e.downcast_ref::<eidetica::Error>()
                .map(failure)
                .unwrap_or(Failure::Failed)
                .text()
        )),
    }
    out.push(format!(
        "TUI cached agent: {} (cache, not routing verdict)",
        if cached.is_empty() {
            "Unavailable"
        } else {
            cached
        }
    ));
    out
}
async fn sharing(db: &Database, registry: &chaz_core::session::SessionRegistry) -> Vec<String> {
    let preference = registry
        .user_lock()
        .await
        .tracked_database(db.root_id())
        .await;
    let mut out = Vec::new();
    if preference.is_ok() {
        out.push(format!(
            "Freshness: sync preference local query snapshot at {}",
            now()
        ));
    }
    out.push(match preference {
        Ok(Some(row)) => format!(
            "Local sync preference: {} (this account only)",
            if row.sync_settings.sync_enabled {
                "enabled"
            } else {
                "disabled"
            }
        ),
        Ok(None) => "Not tracked by this account".into(),
        Err(e) => format!("Local sync preference: {}", failure(&e).text()),
    });
    let key = registry.find_key_for_db(db.root_id()).await;
    if key.is_ok() {
        out.push(format!("Freshness: local key lookup snapshot at {}", now()));
    }
    match key {
        Ok(None) => {
            out.push("No local key found in this account view; direct record not queried".into())
        }
        Err(e) => out.push(format!(
            "Local key lookup: {}; direct record not queried",
            e.downcast_ref::<eidetica::Error>()
                .map(failure)
                .unwrap_or(Failure::Failed)
                .text()
        )),
        Ok(Some(key)) => {
            out.push(format!(
                "Account-held key lookup (first match): {}",
                key.to_prefixed_string()
            ));
            let auth = async { db.get_settings().await?.get_auth_key(&key).await }.await;
            match auth {
                Ok(auth) => {
                    out.push(format!(
                        "Freshness: direct record local query snapshot at {}",
                        now()
                    ));
                    out.push(format!("Direct key status: {:?}", auth.status()));
                    out.push(format!("Direct key permission: {:?}", auth.permissions()));
                }
                Err(e) if matches!(&e, eidetica::Error::Auth(a) if matches!(a.as_ref(), eidetica::auth::AuthError::KeyNotFound { .. })) => {
                    out.push("No direct key record; delegated authority not inspected".into())
                }
                Err(e) => out.push(format!("Direct key record: {}", failure(&e).text())),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_session_inspector_denial_is_not_absence_and_identifiers_are_allowlisted() {
        let error: eidetica::Error = eidetica::auth::AuthError::PermissionDenied {
            reason: "SECRET_RAW_FAILURE".into(),
        }
        .into();
        assert_eq!(failure(&error), Failure::Denied);
        assert_eq!(
            failure(&eidetica::Error::from(std::io::Error::other("SECRET_IO"))),
            Failure::Failed
        );
        let id = eidetica::entry::ID::from_bytes(b"allowlisted DB").to_string();
        assert_eq!(identifier(&id), id);
        assert_eq!(
            identifier("SECRET_INVALID_DB"),
            "Invalid identifier metadata"
        );
        assert_eq!(identifier(""), "Invalid identifier metadata");
        assert!(safe(&"model-name".repeat(30)).ends_with("(truncated)"));
    }
}
