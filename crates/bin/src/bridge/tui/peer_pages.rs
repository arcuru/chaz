//! Entry-time, read-only snapshots for the three static Peer inspectors.
use super::{App, PeerSettingsCategory as Category, SettingsScope, TuiMode, supervised};
use chaz_core::{
    extension::ExtensionRef,
    security::approval_display::{pubkey_fingerprint, quarantine},
    server::Server,
};
use eidetica::auth::crypto::PublicKey;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

#[derive(Debug)]
pub(super) struct Extension {
    pub name: String,
    pub reference: Option<(&'static str, String)>,
    pub hooks: Vec<String>,
    pub tools: Vec<String>,
    pub commands: Vec<String>,
    pub global_failed: bool,
}

#[derive(Debug)]
pub(super) enum Data {
    Extensions(Vec<Extension>),
    Groups {
        templates: Vec<(String, Option<Vec<String>>)>,
        defaults: Vec<String>,
    },
    Identity(Result<PublicIdentity, ReadFailure>),
    Loading,
}

#[derive(Debug)]
pub(super) struct PublicIdentity {
    pub key: String,
    pub fingerprint: String,
    pub observed_at: String,
}

impl From<PublicKey> for PublicIdentity {
    fn from(key: PublicKey) -> Self {
        Self {
            key: key.to_prefixed_string(),
            fingerprint: pubkey_fingerprint(&key),
            observed_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ReadFailure {
    Unavailable,
    Failed,
    Timeout,
}

impl ReadFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Unavailable => "Default public key unavailable: no local key",
            Self::Failed => "Failed to read default public key; diagnostic text withheld",
            Self::Timeout => "Timed out reading default public key",
        }
    }
}

pub(super) struct Snapshot {
    pub category: Category,
    pub data: Data,
}

/// Pointer identity is held by the Arc, so replacement cannot reuse the address.
#[derive(Default)]
pub(super) struct Pages {
    server: Option<Arc<Server>>,
    category: Option<Category>,
    generation: u64,
    pub snapshot: Option<Snapshot>,
    task: Option<supervised::Task<()>>,
}

pub(super) struct Loaded {
    generation: u64,
    result: Result<PublicKey, ReadFailure>,
}

fn selected(app: &App) -> Option<Category> {
    if app.mode != TuiMode::Settings(SettingsScope::Peer) {
        return None;
    }
    match Category::ALL.get(app.peer_settings_index) {
        Some(c @ (Category::Extensions | Category::Groups | Category::Identity)) => Some(*c),
        _ => None,
    }
}

/// Called by the event loop, never by rendering. Same-page events
/// reuse the snapshot; leaving/re-entering or replacing Server invalidates it.
pub(super) fn update(app: &mut App, server: &Arc<Server>, tx: &mpsc::Sender<Loaded>) {
    let category = selected(app);
    let pages = &mut app.peer_pages;
    if pages.category == category
        && (category.is_none()
            || pages
                .server
                .as_ref()
                .is_some_and(|old| Arc::ptr_eq(old, server)))
    {
        return;
    }
    pages.task = None;
    pages.generation = pages.generation.wrapping_add(1);
    pages.snapshot = None;
    pages.category = category;
    pages.server = category.map(|_| server.clone());
    let Some(category) = category else {
        return;
    };
    let data = match category {
        Category::Extensions => {
            let hub = server.extensions();
            let refs = hub.extension_refs();
            let failures = hub.install_failures();
            let mut names = hub.extension_names();
            names.sort_unstable();
            Data::Extensions(
                names
                    .into_iter()
                    .map(|name| {
                        let reference = refs.iter().find(|r| r.name() == name).map(|r| {
                            let kind = match r {
                                ExtensionRef::Builtin { .. } => "Builtin",
                                ExtensionRef::Eidetica { .. } => "Eidetica",
                                ExtensionRef::Ipld { .. } => "Ipld",
                                ExtensionRef::Git { .. } => "Git",
                            };
                            (kind, safe(r.version()))
                        });
                        let mut hooks: Vec<_> = hub
                            .hooks_for(name)
                            .into_iter()
                            .map(|h| format!("{h:?}"))
                            .collect();
                        let mut tools: Vec<_> = hub.tools_for(name).into_iter().collect();
                        let mut commands: Vec<_> = hub.commands_for(name).into_iter().collect();
                        hooks.sort();
                        tools.sort();
                        commands.sort();
                        Extension {
                            name: safe(name),
                            reference,
                            hooks,
                            tools: tools.iter().map(|s| safe(s)).collect(),
                            commands: commands.iter().map(|s| safe(s)).collect(),
                            global_failed: failures.iter().any(|f| f.name == name),
                        }
                    })
                    .collect(),
            )
        }
        Category::Groups => {
            let names = server.agent_group_names();
            Data::Groups {
                templates: names
                    .into_iter()
                    .map(|name| {
                        let members = server
                            .agent_group(&name)
                            .map(|members| members.iter().map(|s| safe(s)).collect());
                        (safe(&name), members)
                    })
                    .collect(),
                defaults: server.default_agents().iter().map(|s| safe(s)).collect(),
            }
        }
        Category::Identity => {
            let registry = server.registry_arc();
            let generation = pages.generation;
            let tx = tx.clone();
            pages.task = Some(supervised::Task(tokio::spawn(async move {
                // The native lookup only fails when the held-key map is empty;
                // it does not query storage or establish authorization denial.
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    supervised::work(async move { Ok(registry.default_pubkey().await.ok()) }),
                )
                .await;
                let result = match result {
                    Ok(Ok(Some(key))) => Ok(key),
                    Ok(Ok(None)) => Err(ReadFailure::Unavailable),
                    Ok(Err(_)) => Err(ReadFailure::Failed),
                    Err(_) => Err(ReadFailure::Timeout),
                };
                let _ = tx.send(Loaded { generation, result }).await;
            })));
            Data::Loading
        }
        _ => unreachable!(),
    };
    pages.snapshot = Some(Snapshot { category, data });
}

pub(super) fn apply(app: &mut App, loaded: Loaded) {
    if selected(app) != Some(Category::Identity) || loaded.generation != app.peer_pages.generation {
        return;
    }
    app.peer_pages.task = None;
    app.peer_pages.snapshot = Some(Snapshot {
        category: Category::Identity,
        data: Data::Identity(loaded.result.map(PublicIdentity::from)),
    });
}

fn safe(raw: &str) -> String {
    let value = quarantine(raw);
    format!("{}{}", value.text, value.note())
}

/// Only allowlisted display values survive into a snapshot. Raw install errors,
/// ref locations, settings, credentials and config never enter the page body.
pub(super) fn lines(category: Category, snapshot: Option<&Snapshot>) -> Vec<String> {
    let mut out = vec![format!("{} — read-only", category.label())];
    out.push(
        match category {
            Category::Extensions => "Scope: this process registration catalog",
            Category::Groups => "Scope: loaded new-session roster templates",
            Category::Identity => "Scope: connected account default public key",
            _ => unreachable!(),
        }
        .into(),
    );
    if let Some(snapshot) = snapshot.filter(|s| s.category == category) {
        match (&snapshot.data, category) {
            (Data::Extensions(entries), Category::Extensions) => {
                out.push("Freshness: process snapshot; reopen to refresh".into());
                out.push(format!("Registered extensions: {}", entries.len()));
                if entries.is_empty() {
                    out.push("No registered extensions in this process".into());
                }
                for e in entries {
                    out.push(format!("Extension: {}", e.name));
                    out.push(match &e.reference {
                        Some((kind, version)) => format!("Reference: {kind}; version: {version}"),
                        None => "Reference unavailable".into(),
                    });
                    for (label, values) in [
                        ("hooks", &e.hooks),
                        ("tools", &e.tools),
                        ("commands", &e.commands),
                    ] {
                        if values.is_empty() {
                            out.push(format!("No registered {label}"));
                        } else {
                            out.push(format!("Registered {label}:"));
                            out.extend(values.iter().map(|s| format!("  {s}")));
                        }
                    }
                    out.push(
                        if e.global_failed {
                            "Global install failed; diagnostic text withheld"
                        } else {
                            "No recorded Global install failure"
                        }
                        .into(),
                    );
                }
            }
            (
                Data::Groups {
                    templates,
                    defaults,
                },
                Category::Groups,
            ) => {
                out.push("Freshness: process snapshot; reopen to refresh".into());
                out.push(format!("Named templates: {}", templates.len()));
                if templates.is_empty() {
                    out.push("No named groups loaded".into());
                }
                for (name, members) in templates {
                    out.push(format!("Template: {name}"));
                    match members {
                        None => out.push("Roster template unavailable".into()),
                        Some(members) if members.is_empty() => {
                            out.push("Empty roster template".into())
                        }
                        Some(members) => {
                            out.push("Ordered members:".into());
                            out.extend(
                                members
                                    .iter()
                                    .enumerate()
                                    .map(|(i, m)| format!("  {}. {m}", i + 1)),
                            );
                        }
                    }
                }
                out.push("Current default new-session roster:".into());
                if defaults.is_empty() {
                    out.push("Default roster list empty; creation may use legacy default".into());
                } else {
                    out.extend(
                        defaults
                            .iter()
                            .enumerate()
                            .map(|(i, m)| format!("  {}. {m}", i + 1)),
                    );
                }
            }
            (Data::Loading, Category::Identity) => out.push("Loading default public key…".into()),
            (Data::Identity(Ok(key)), Category::Identity) => {
                out.push(format!(
                    "Freshness: local query snapshot at {}; reopen to refresh",
                    key.observed_at
                ));
                out.push("Default public key:".into());
                out.push(key.key.clone());
                out.push(format!("Fingerprint: {}", key.fingerprint));
            }
            (Data::Identity(Err(e)), Category::Identity) => {
                out.push(e.message().into());
                out.push("Fingerprint unavailable: no default public-key snapshot".into());
            }
            _ => out.push("Page snapshot unavailable".into()),
        }
    } else {
        out.push("Page snapshot unavailable".into());
    }
    out.extend(match category {
        Category::Extensions => vec![
            "Catalog only. Agent/session instances and effective activation are not inspected here. MCP runtime status is on Peer MCP.",
            "Esc back; /extensions list inspects the current session/responding agent, not process-wide activity.",
        ],
        Category::Groups => vec![
            "New-session templates; not live groups or the current session roster.",
            "Members are not guaranteed attachments. An explicitly empty named group attaches no agents.",
            "Config files are not hot-reloaded by this page.",
            "Esc back; /groups lists templates; /new <group> creates a session (mutates). No group editor here.",
        ],
        Category::Identity => vec![
            "Default account public key; agent/session keys may differ. Private key material is never shown.",
            "Not a transport peer id or effective session signer. No key editor or rotation here.",
            "Esc back; /pubkey prints the default public key.",
        ],
        _ => unreachable!(),
    }.into_iter().map(String::from));
    out.push("Leave/reopen this page to retry; check the existing command's result".into());
    out
}
