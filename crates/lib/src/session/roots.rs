//! Direct references to the registry's two bookkeeping roots.
//!
//! `chaz_group` and `chaz_peer` are opened by the IDs this login's Eidetica
//! Account records under the `chaz` reference namespace, never by searching
//! tracked databases for a display name. Lookup reads two references and
//! opens two databases, however many other databases the login tracks.
//!
//! - `chaz.group` names the group database.
//! - `chaz.peer-<login id>` names this installation's peer database. The
//!   login ID is the instance-local login record, not the Account's root ID,
//!   so a peer reference written by another installation of the same Account
//!   can never select this installation's peer runtime state.
//!
//! Absence is the only condition that may lead to creation, and only for the
//! designated writer ([`RootSetup::Initialize`]). A present reference that
//! cannot be read or opened is an error: Chaz never replaces it. Before
//! creating a root, the initializer refuses if the login already tracks an
//! unreferenced database with the root's display name — state from an older
//! Chaz, or a target whose reference write failed — and asks the operator to
//! record that ID explicitly with `chaz roots`.

use std::fmt;

use eidetica::entry::ID;
use eidetica::{AccountSession, Database};

/// Reference namespace Chaz owns in its login's Account.
pub const REFERENCE_NAMESPACE: &str = "chaz";
const GROUP_REFERENCE: &str = "group";
const PEER_REFERENCE_PREFIX: &str = "peer-";

/// Serializes in-process initialization. Across processes, the designated
/// writer contract does: a direct owner holds its storage exclusively, and a
/// service login has exactly one executor.
static INITIALIZE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One of the registry's two bookkeeping roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    Group,
    Peer,
}

impl RootKind {
    /// Display name given to a created root. Also the name an older Chaz
    /// searched for, which the initializer checks before creating.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Group => "chaz_group",
            Self::Peer => "chaz_peer",
        }
    }

    /// `chaz roots` flag that records this root's ID.
    pub fn flag(self) -> &'static str {
        match self {
            Self::Group => "--group",
            Self::Peer => "--peer",
        }
    }

    /// Reference name for this root under [`REFERENCE_NAMESPACE`].
    pub fn reference_name(self, user: &AccountSession) -> String {
        match self {
            Self::Group => GROUP_REFERENCE.to_string(),
            Self::Peer => format!("{PEER_REFERENCE_PREFIX}{}", user.login_id()),
        }
    }
}

impl fmt::Display for RootKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Whether absent roots may be created by this caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootSetup {
    /// The designated writer: a direct owner of its storage, or the single
    /// executor of a service login. Absent roots are created and referenced.
    Initialize,
    /// Any other caller. Absent roots are an error.
    RequireExisting,
}

impl RootSetup {
    /// The designated writer for a connection: the exclusive owner of a
    /// direct backend, or the executor of a service login.
    pub fn for_connection(instance: &eidetica::Instance, executor: bool) -> Self {
        if executor || instance.remote_connection().is_none() {
            Self::Initialize
        } else {
            Self::RequireExisting
        }
    }
}

/// The two opened roots.
pub struct Roots {
    pub group: Database,
    pub peer: Database,
}

fn ids(ids: &[ID]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn kinds(kinds: &[RootKind]) -> String {
    kinds
        .iter()
        .map(|kind| kind.display_name())
        .collect::<Vec<_>>()
        .join(", ")
}

fn unreferenced_list(found: &[(RootKind, Vec<ID>)]) -> String {
    found
        .iter()
        .map(|(kind, candidates)| format!("{kind} {}", ids(candidates)))
        .collect::<Vec<_>>()
        .join("; ")
}

/// `chaz roots` invocation for the found candidates; `<ID>` where a kind has
/// several and the operator must choose.
fn record_command(found: &[(RootKind, Vec<ID>)]) -> String {
    let mut command = String::from("chaz roots");
    for (kind, candidates) in found {
        let id = match candidates.as_slice() {
            [only] => only.to_string(),
            _ => "<ID>".to_string(),
        };
        command.push_str(&format!(" {} {id}", kind.flag()));
    }
    command
}

#[derive(Debug, thiserror::Error)]
pub enum RootsError {
    /// The reference exists but its value or target cannot be read, opened
    /// or authorized. Never treated as absence.
    #[error(
        "Chaz {kind} reference is present but unusable: {source}. It was not replaced; repair the referenced database or its access"
    )]
    Unavailable {
        kind: RootKind,
        #[source]
        source: eidetica::Error,
    },
    /// Absent roots on a caller that is not the designated writer.
    #[error(
        "Chaz bookkeeping roots are not initialized for this login (missing: {}). Start the direct owner or the login's executor first; it initializes them", kinds(.missing)
    )]
    NotInitialized { missing: Vec<RootKind> },
    /// The login tracks unreferenced database(s) carrying a root's name.
    /// Nothing was created.
    #[error(
        "found unreferenced Chaz root database(s): {}; nothing was created. With no other Chaz process running for this login, record the intended IDs with `{}`", unreferenced_list(.found), record_command(.found)
    )]
    Unreferenced { found: Vec<(RootKind, Vec<ID>)> },
    /// A root was created but its reference could not be written.
    #[error(
        "created {kind} database {id} but could not record its reference: {source}. Record it with `chaz roots {} {id}`; do not expect a restart to create another", .kind.flag()
    )]
    PartialInitialization {
        kind: RootKind,
        id: ID,
        #[source]
        source: eidetica::Error,
    },
    /// `chaz roots` asked to record an ID over a different present reference.
    #[error("Chaz {kind} reference already names {existing}; it was not changed to {requested}")]
    AlreadyReferenced {
        kind: RootKind,
        existing: ID,
        requested: ID,
    },
    /// A tracked database could be an old root, but could not be inspected.
    #[error(
        "could not inspect tracked database {id} before initializing Chaz {kind}: {source}. Nothing was created; repair its data or access before retrying"
    )]
    Inspect {
        kind: RootKind,
        id: ID,
        #[source]
        source: eidetica::Error,
    },
    /// Any other failure while checking for or creating a root.
    #[error("could not initialize Chaz {kind}: {source}")]
    Initialize {
        kind: RootKind,
        #[source]
        source: eidetica::Error,
    },
}

/// Open a root by its reference. `Ok(None)` only for an absent reference.
async fn lookup(user: &AccountSession, kind: RootKind) -> Result<Option<Database>, RootsError> {
    user.open_reference(REFERENCE_NAMESPACE, &kind.reference_name(user))
        .await
        .map_err(|source| RootsError::Unavailable { kind, source })
}

/// Open both roots, initializing absent ones only under [`RootSetup::Initialize`].
pub async fn open(user: &mut AccountSession, setup: RootSetup) -> Result<Roots, RootsError> {
    let group = lookup(user, RootKind::Group).await?;
    let peer = lookup(user, RootKind::Peer).await?;
    if let (Some(group), Some(peer)) = (&group, &peer) {
        return Ok(Roots {
            group: group.clone(),
            peer: peer.clone(),
        });
    }
    if setup == RootSetup::RequireExisting {
        let missing = [
            (RootKind::Group, group.is_none()),
            (RootKind::Peer, peer.is_none()),
        ]
        .into_iter()
        .filter_map(|(kind, absent)| absent.then_some(kind))
        .collect();
        return Err(RootsError::NotInitialized { missing });
    }

    let _initializing = INITIALIZE.lock().await;
    // Another in-process initializer may have finished while this one waited.
    let group = lookup(user, RootKind::Group).await?;
    let peer = lookup(user, RootKind::Peer).await?;
    // Check every absent root before creating any, so one refusal names all
    // the IDs the operator must record.
    let mut found = Vec::new();
    for (kind, absent) in [
        (RootKind::Group, group.is_none()),
        (RootKind::Peer, peer.is_none()),
    ] {
        if absent {
            let candidates = unreferenced(user, kind).await?;
            if !candidates.is_empty() {
                found.push((kind, candidates));
            }
        }
    }
    if !found.is_empty() {
        return Err(RootsError::Unreferenced { found });
    }
    let group = match group {
        Some(db) => db,
        None => create(user, RootKind::Group).await?,
    };
    let peer = match peer {
        Some(db) => db,
        None => create(user, RootKind::Peer).await?,
    };
    Ok(Roots { group, peer })
}

/// Unreferenced tracked databases carrying `kind`'s display name.
///
/// This is the only name search left, and it runs only when a designated
/// writer is about to create a root.
async fn unreferenced(user: &AccountSession, kind: RootKind) -> Result<Vec<ID>, RootsError> {
    let tracked = user
        .databases()
        .await
        .map_err(|source| RootsError::Initialize { kind, source })?;
    let mut found = Vec::new();
    for tracked in tracked {
        // find_database skips failed opens and name reads. Setup must prove
        // absence rather than create a replacement for an unreadable old root.
        let matches = async {
            let db = user.open_database(&tracked.database_id).await?;
            match db.get_name().await {
                Ok(name) => Ok(name == kind.display_name()),
                // An absent name proves this is not a legacy named root.
                Err(eidetica::Error::Store(error))
                    if matches!(error.as_ref(), eidetica::store::StoreError::KeyNotFound { store, key }
                        if store == "_settings" && key == "name") => Ok(false),
                Err(error) => Err(error),
            }
        }
        .await
        .map_err(|source| RootsError::Inspect {
            kind,
            id: tracked.database_id.clone(),
            source,
        })?;
        if matches {
            found.push(tracked.database_id);
        }
    }
    Ok(found)
}

async fn create(user: &mut AccountSession, kind: RootKind) -> Result<Database, RootsError> {
    let mut settings = eidetica::crdt::Doc::new();
    settings.set("name", kind.display_name());
    let created = async {
        let key = user.get_default_key()?;
        user.create_database(settings, &key).await
    }
    .await
    .map_err(|source| RootsError::Initialize { kind, source })?;
    user.set_reference(
        REFERENCE_NAMESPACE,
        &kind.reference_name(user),
        created.root_id(),
    )
    .await
    .map_err(|source| RootsError::PartialInitialization {
        kind,
        id: created.root_id().clone(),
        source,
    })?;
    tracing::info!(root = %kind, db_id = %created.root_id(), "Initialized Chaz bookkeeping root");
    Ok(created)
}

/// Explicitly record `id` as this login's `kind` root.
///
/// For state from an older Chaz, or a root whose reference write failed. The
/// target must open with this login's keys. A present reference is never
/// changed: recording the ID it already names is a no-op, any other ID is
/// refused.
pub async fn record(user: &AccountSession, kind: RootKind, id: &ID) -> Result<(), RootsError> {
    let name = kind.reference_name(user);
    match user.reference(REFERENCE_NAMESPACE, &name).await {
        Ok(Some(existing)) if &existing == id => return Ok(()),
        Ok(Some(existing)) => {
            return Err(RootsError::AlreadyReferenced {
                kind,
                existing,
                requested: id.clone(),
            });
        }
        Ok(None) => {}
        Err(source) => return Err(RootsError::Unavailable { kind, source }),
    }
    user.open_database(id)
        .await
        .map_err(|source| RootsError::Initialize { kind, source })?;
    user.set_reference(REFERENCE_NAMESPACE, &name, id)
        .await
        .map_err(|source| RootsError::Initialize { kind, source })
}

/// The recorded ID for `kind`, without opening it.
pub async fn referenced(user: &AccountSession, kind: RootKind) -> Result<Option<ID>, RootsError> {
    user.reference(REFERENCE_NAMESPACE, &kind.reference_name(user))
        .await
        .map_err(|source| RootsError::Unavailable { kind, source })
}
