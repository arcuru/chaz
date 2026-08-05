//! Bridge identity + access bootstrap (core flow; live P2P behind a seam).
//!
//! A standalone bridge runs its **own** eidetica `User` and holds its **own**
//! key — it is not chaz's peer key. This module covers the bridge's
//! agent-facing bring-up:
//!
//! 1. [`ensure_bridge_key`] — self-generate and persist that key on first run,
//!    reuse it thereafter (stable identity across restarts).
//! 2. [`AccessBootstrap`] — the seam over eidetica's ticket-based access
//!    request. The live [`SyncBootstrap`] drives `User::request_database_access`
//!    against the peer addresses carried in a [`DatabaseTicket`] (the same
//!    path `/agent share` → `/agent import` uses); tests substitute a stub so
//!    the orchestration is exercised without standing up P2P sync. The request
//!    resolves to a [`BootstrapOutcome`]: `Approved` when the bridge's key was
//!    already authorized, `Pending` when the owner must approve via
//!    `/sharing approve` first (the bridge retries later).
//! 3. [`establish_login`] — request `Read` on the owning agent's DB via a
//!    ticket, carrying the public [`LoginRef`](crate::agent_db::LoginRef)
//!    pointer as bootstrap metadata. The approving owner reads the pointer off
//!    that metadata and registers it, so the bridge never writes to the agent
//!    DB itself.
//!
//! `Read` suffices on the agent DB because the bridge no longer self-registers
//! its pointer: eidetica carries free-form metadata on a bootstrap request, so
//! the owner learns the bridge-created settings-DB id at request time and
//! writes the registry entry. A proxy-writer holding `Write` on the agent's own
//! DB was more authority than the design wanted. The bridge still needs `Write`
//! on **session** DBs, to proxy-write inbound transport messages.

#![allow(dead_code)]

use crate::agent_db::{AgentDb, LoginRef};
use crate::session::BootstrapOutcome;
use eidetica::auth::crypto::PublicKey;
use eidetica::auth::types::Permission;
use eidetica::crdt::Doc;
use eidetica::sync::{DatabaseTicket, Sync, SyncError};
use eidetica::user::User;
use std::sync::Arc;
use tracing::{info, warn};

/// Display name carried by a bridge's own eidetica key — also the
/// `requesting_key_name` used when bootstrapping access.
pub const BRIDGE_KEY_NAME: &str = "bridge";

/// Ensure the bridge holds its own persistent eidetica key, generating and
/// persisting one on first run. Idempotent: an existing key with `key_name`
/// is reused, so the bridge keeps a stable identity across restarts rather
/// than minting a fresh pubkey (and needing re-approval) every boot.
pub async fn ensure_bridge_key(user: &mut User, key_name: &str) -> anyhow::Result<PublicKey> {
    if let Some(existing) = user.find_keys_by_display_name(key_name).into_iter().next() {
        return Ok(existing);
    }
    let key = user.add_private_key(Some(key_name)).await?;
    info!(key = %key, key_name, "Generated bridge identity key");
    Ok(key)
}

/// Seam over eidetica's ticket-based access request. The real implementation
/// ([`SyncBootstrap`]) drives `User::request_database_access`; tests provide a
/// stub so [`establish_login`] can be exercised without P2P sync.
#[allow(async_fn_in_trait)]
pub trait AccessBootstrap {
    /// Request `permission` on the database `ticket` points at, authenticating
    /// as the bridge `user`'s `key`. Resolves to
    /// [`BootstrapOutcome::Approved`] once access is in hand (the owning peer
    /// had pre-authorized the key and the DB synced locally), or
    /// [`BootstrapOutcome::Pending`] when the owner must approve the queued
    /// request first. Errors only on transport/protocol failure.
    async fn request_access(
        &self,
        user: &mut User,
        ticket: &DatabaseTicket,
        key: &PublicKey,
        permission: Permission,
        metadata: Option<Doc>,
    ) -> anyhow::Result<BootstrapOutcome>;
}

/// Live `AccessBootstrap` over an eidetica `Sync` handle. Drives
/// `User::request_database_access`; the owner-side approval is the existing
/// chaz `/sharing` flow. Retry-until-approved is the caller/binary's concern
/// (the request stays queued on the owner, and a `Pending` outcome tells the
/// binary to come back later).
///
/// Address hints are tried **one at a time**, in ticket order, rather than
/// handing the whole ticket to eidetica at once. Eidetica races every hint
/// concurrently — right for reachability, wrong for request filing: each racer
/// runs a full bootstrap round-trip and files its own request, so a two-hint
/// ticket leaves two entries in the owner's approval queue with identical
/// `(database, key, permission)`. The requester keeps only the last outcome, so
/// approving one leaves the other pending forever. Splitting the ticket costs
/// the fallback's latency (a dead first hint must time out before the second is
/// tried) and buys exactly one queue entry per bootstrap.
///
/// Only a transport failure advances to the next hint. A `Pending` outcome is a
/// definitive answer — the request is filed — so we stop there rather than
/// filing a duplicate against the same peer by another route.
pub struct SyncBootstrap {
    pub sync: Arc<Sync>,
}

impl SyncBootstrap {
    pub fn new(sync: Arc<Sync>) -> Self {
        Self { sync }
    }
}

impl AccessBootstrap for SyncBootstrap {
    async fn request_access(
        &self,
        user: &mut User,
        ticket: &DatabaseTicket,
        key: &PublicKey,
        permission: Permission,
        metadata: Option<Doc>,
    ) -> anyhow::Result<BootstrapOutcome> {
        let mut last_err = None;
        for attempt in single_address_tickets(ticket) {
            match user
                .request_database_access(&self.sync, &attempt, key, permission, metadata.clone())
                .await
            {
                Ok(()) => return Ok(BootstrapOutcome::Approved),
                Err(e) => {
                    if let eidetica::Error::Sync(boxed) = &e
                        && let SyncError::BootstrapPending {
                            request_id,
                            message,
                        } = boxed.as_ref()
                    {
                        // Filed. Stop — another hint would file a duplicate.
                        return Ok(BootstrapOutcome::Pending {
                            request_id: request_id.clone(),
                            message: message.clone(),
                        });
                    }
                    if let Some(addr) = attempt.addresses().first() {
                        warn!(address = ?addr, "Bootstrap attempt failed, trying next hint: {e}");
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(match last_err {
            Some(e) => e.into(),
            None => anyhow::anyhow!("ticket carries no address hints to bootstrap against"),
        })
    }
}

/// Split a ticket into one single-hint ticket per address, preserving order.
/// A ticket with no hints yields nothing — there is nothing to dial.
fn single_address_tickets(ticket: &DatabaseTicket) -> Vec<DatabaseTicket> {
    ticket
        .addresses()
        .iter()
        .map(|addr| {
            DatabaseTicket::with_addresses(ticket.database_id().clone(), vec![addr.clone()])
        })
        .collect()
}

/// Identity of the bridge issuing a bootstrap request: its own key plus the
/// display name that key carries (also the `requesting_key_name`).
pub struct BridgeIdentity<'a> {
    pub key: &'a PublicKey,
    pub key_name: &'a str,
}

/// Bring one bridged login online against its owning agent:
///
/// 1. request `Read` on the agent DB the `ticket` points at (via the
///    [`AccessBootstrap`] seam), carrying the [`LoginRef`] as bootstrap
///    metadata,
/// 2. if the request is still `Pending` owner approval, return that outcome —
///    nothing is registered yet and the binary retries after approval,
/// 3. otherwise confirm the owner registered the pointer on our behalf.
///
/// The bridge holds `Read` here, not `Write`: it does not write its own
/// pointer into the agent DB, the approving owner does, reading it off the
/// bootstrap metadata. A proxy-writer needs no write authority on the agent's
/// own database. `Write` on *session* DBs is separate and unchanged — that is
/// where the bridge proxies inbound transport messages.
///
/// The secret details were already seeded into the bridge's own DB (the
/// bridge manages that); the pointer published here is non-secret.
pub async fn establish_login<B: AccessBootstrap>(
    user: &mut User,
    bootstrap: &B,
    identity: &BridgeIdentity<'_>,
    ticket: &DatabaseTicket,
    login: LoginRef,
) -> anyhow::Result<BootstrapOutcome> {
    let outcome = bootstrap
        .request_access(
            user,
            ticket,
            identity.key,
            Permission::Read,
            Some(login.to_metadata()?),
        )
        .await?;
    if let BootstrapOutcome::Pending { request_id, .. } = &outcome {
        info!(
            agent_db = %ticket.database_id(),
            login = %login.identifier,
            request_id = %request_id,
            "Bridge login access pending owner approval; will retry after approval"
        );
        return Ok(outcome);
    }
    // Approved. `request_database_access` tracks the database with sync
    // **disabled** by default, so without this the agent DB is openable but
    // never converges — neither eidetica's background engine nor chaz's keyed
    // reconciler looks at a database whose owner has not asked for sync. A
    // bridge always wants its agent DB syncing; that is the whole point of
    // bootstrapping it.
    //
    // Best-effort: `track_database` re-discovers the SigKey and fails if this
    // key holds no authority on the database. That cannot happen on the path
    // that just approved us, but a caller whose access arrived some other way
    // should get a loud warning and a working login rather than a hard abort.
    let agent_db_id = ticket.database_id();
    if let Err(e) = user
        .track_database(
            agent_db_id.clone(),
            identity.key,
            eidetica::user::types::SyncSettings::on_commit(),
        )
        .await
    {
        warn!(
            agent_db = %agent_db_id,
            "Could not enable sync on the agent DB ({e}); it will open locally but \
             never receive the daemon's writes"
        );
    }

    // The owner writes the pointer when it approves a queued
    // request, so on that path it is already there. A key the owner had
    // pre-authorized (e.g. via `/agent invite`) is approved without ever
    // queueing a request, so nothing observed our metadata — and holding only
    // `Read` we cannot write the pointer ourselves. Say so plainly rather
    // than leaving the login silently undiscoverable.
    let database = user.open_database(agent_db_id).await?;
    let agent_db = AgentDb::from_database(database);
    if agent_db.find_login(&login.identifier).await?.is_some() {
        info!(
            agent_db = %agent_db_id,
            login = %login.identifier,
            "Bridge login pointer present in agent DB"
        );
    } else {
        warn!(
            agent_db = %agent_db_id,
            login = %login.identifier,
            bridge_db = %login.bridge_db_id,
            "Access was pre-authorized, so no bootstrap request carried this \
             login's pointer and the owner never registered it. The login works \
             but peers cannot discover it. To publish the pointer, revoke this \
             bridge's pre-authorization on the owning peer and let it bootstrap \
             through the normal `/sharing approve` path."
        );
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
    use crate::bridge_db::create_bridge_db;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};

    async fn test_user() -> User {
        let backend = InMemory::new();
        let (_instance, user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
        user
    }

    /// What a stub bootstrap was asked for, so tests can assert on the
    /// permission and metadata the bridge actually requests.
    #[derive(Default)]
    struct Recorded {
        permission: Option<Permission>,
        metadata: Option<Doc>,
    }

    /// A no-op bootstrap: access is assumed already in hand. Lets the
    /// orchestration be tested without live sync (the single-user test already
    /// holds admin on the agent DB it created). Records the ask.
    #[derive(Default)]
    struct GrantedBootstrap {
        seen: std::sync::Mutex<Recorded>,
    }
    impl AccessBootstrap for GrantedBootstrap {
        async fn request_access(
            &self,
            _user: &mut User,
            _ticket: &DatabaseTicket,
            _key: &PublicKey,
            permission: Permission,
            metadata: Option<Doc>,
        ) -> anyhow::Result<BootstrapOutcome> {
            let mut seen = self.seen.lock().unwrap();
            seen.permission = Some(permission);
            seen.metadata = metadata;
            Ok(BootstrapOutcome::Approved)
        }
    }

    /// A bootstrap stuck awaiting owner approval — proves `establish_login`
    /// reports `Pending` and registers nothing yet.
    struct PendingBootstrap;
    impl AccessBootstrap for PendingBootstrap {
        async fn request_access(
            &self,
            _user: &mut User,
            _ticket: &DatabaseTicket,
            _key: &PublicKey,
            _permission: Permission,
            _metadata: Option<Doc>,
        ) -> anyhow::Result<BootstrapOutcome> {
            Ok(BootstrapOutcome::Pending {
                request_id: "req-123".to_string(),
                message: "awaiting approval".to_string(),
            })
        }
    }

    /// A bootstrap that always fails — proves `establish_login` aborts before
    /// touching the agent DB when access can't be obtained.
    struct DeniedBootstrap;
    impl AccessBootstrap for DeniedBootstrap {
        async fn request_access(
            &self,
            _user: &mut User,
            _ticket: &DatabaseTicket,
            _key: &PublicKey,
            _permission: Permission,
            _metadata: Option<Doc>,
        ) -> anyhow::Result<BootstrapOutcome> {
            anyhow::bail!("access denied")
        }
    }

    #[tokio::test]
    async fn ensure_bridge_key_is_idempotent() {
        let mut user = test_user().await;
        let k1 = ensure_bridge_key(&mut user, BRIDGE_KEY_NAME).await.unwrap();
        let k2 = ensure_bridge_key(&mut user, BRIDGE_KEY_NAME).await.unwrap();
        assert_eq!(k1, k2, "second call must reuse the existing bridge key");
        assert_eq!(
            user.find_keys_by_display_name(BRIDGE_KEY_NAME).len(),
            1,
            "no duplicate bridge key minted"
        );
    }

    #[tokio::test]
    async fn establish_login_requests_read_and_carries_the_pointer() {
        let mut user = test_user().await;
        let bridge_key = ensure_bridge_key(&mut user, BRIDGE_KEY_NAME).await.unwrap();
        let (agent_db, _) = create_agent_db(
            &mut user,
            "chaz",
            &AgentDbConfig::default(),
            &AgentMeta {
                display_name: Some("chaz".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let agent_db_id = agent_db.id();
        let (bridge_db, _) = create_bridge_db(&mut user, "matrix").await.unwrap();
        let settings_db_id = bridge_db.id();

        let bootstrap = GrantedBootstrap::default();
        let outcome = establish_login(
            &mut user,
            &bootstrap,
            &BridgeIdentity {
                key: &bridge_key,
                key_name: BRIDGE_KEY_NAME,
            },
            &DatabaseTicket::new(agent_db_id),
            LoginRef {
                kind: "matrix".to_string(),
                identifier: "@chaz:example".to_string(),
                bridge_db_id: settings_db_id.to_string(),
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, BootstrapOutcome::Approved));

        let (permission, metadata) = {
            let seen = bootstrap.seen.lock().unwrap();
            (seen.permission, seen.metadata.clone())
        };

        // The bridge asks for Read, not Write: the owner writes the pointer.
        assert_eq!(
            permission,
            Some(Permission::Read),
            "bridge must not request write authority on the agent DB"
        );

        // ...and hands the owner the pointer to register, via metadata.
        let carried = LoginRef::from_metadata(
            metadata
                .as_ref()
                .expect("request must carry login metadata"),
        )
        .expect("metadata must decode back to the LoginRef");
        assert_eq!(carried.kind, "matrix");
        assert_eq!(carried.identifier, "@chaz:example");
        assert_eq!(carried.bridge_db_id, settings_db_id.to_string());

        // Nothing self-registered: this stub models a pre-authorized key, so
        // no request was queued for an owner to approve and act on.
        assert!(agent_db.list_logins().await.unwrap().is_empty());
    }

    #[test]
    fn a_multi_hint_ticket_splits_into_one_ticket_per_hint() {
        use eidetica::sync::Address;
        let db_id = eidetica::entry::ID::parse(
            "bafyr4ihvgcnzzmbaxmzjact36ggoa42z6r5cpe2anlthvl7ajfm2td7e3y",
        )
        .unwrap();
        let http = Address::http("127.0.0.1:9801");
        let iroh = Address::iroh("endpointabx2tmyeynpdg27kdcu55gwrqgwavj66");
        let ticket =
            DatabaseTicket::with_addresses(db_id.clone(), vec![http.clone(), iroh.clone()]);

        let split = single_address_tickets(&ticket);

        // One dial per hint, so the owner sees at most one queued request per
        // bootstrap instead of one per hint.
        assert_eq!(split.len(), 2);
        // Ticket order is preserved — the first hint is tried first, and the
        // second is only reached if the first fails at the transport level.
        assert_eq!(split[0].addresses(), &[http]);
        assert_eq!(split[1].addresses(), &[iroh]);
        for t in &split {
            assert_eq!(t.database_id(), &db_id);
        }
    }

    #[test]
    fn a_ticket_with_no_hints_yields_no_attempts() {
        // Nothing to dial: `request_access` must report that rather than
        // silently reporting success or looping.
        assert!(
            single_address_tickets(&DatabaseTicket::new(
                eidetica::entry::ID::parse(
                    "bafyr4ihvgcnzzmbaxmzjact36ggoa42z6r5cpe2anlthvl7ajfm2td7e3y"
                )
                .unwrap()
            ))
            .is_empty()
        );
    }

    #[tokio::test]
    async fn login_ref_metadata_round_trips() {
        let login = LoginRef {
            kind: "discord".to_string(),
            identifier: "chaz#4242".to_string(),
            bridge_db_id: "bafyrbridgedb".to_string(),
        };
        let decoded = LoginRef::from_metadata(&login.to_metadata().unwrap()).unwrap();
        assert_eq!(decoded, login);
    }

    #[tokio::test]
    async fn unrelated_metadata_is_not_read_as_a_login() {
        // Every non-bridge access request goes through the same channel, so an
        // absent or foreign payload must simply yield no login rather than
        // erroring the approval.
        assert!(LoginRef::from_metadata(&Doc::new()).is_none());
        let mut other = Doc::new();
        other.set("some_other_key", "value");
        assert!(LoginRef::from_metadata(&other).is_none());
    }

    #[tokio::test]
    async fn establish_login_pending_does_not_register() {
        let mut user = test_user().await;
        let bridge_key = ensure_bridge_key(&mut user, BRIDGE_KEY_NAME).await.unwrap();
        let (agent_db, _) = create_agent_db(
            &mut user,
            "chaz",
            &AgentDbConfig::default(),
            &AgentMeta::default(),
        )
        .await
        .unwrap();
        let agent_db_id = agent_db.id();
        let (bridge_db, _) = create_bridge_db(&mut user, "matrix").await.unwrap();
        let settings_db_id = bridge_db.id();

        let outcome = establish_login(
            &mut user,
            &PendingBootstrap,
            &BridgeIdentity {
                key: &bridge_key,
                key_name: BRIDGE_KEY_NAME,
            },
            &DatabaseTicket::new(agent_db_id),
            LoginRef {
                kind: "matrix".to_string(),
                identifier: "@chaz:example".to_string(),
                bridge_db_id: settings_db_id.to_string(),
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, BootstrapOutcome::Pending { .. }));
        // Pending must not publish the pointer — only an approved request does.
        assert!(agent_db.list_logins().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn establish_login_aborts_when_access_denied() {
        let mut user = test_user().await;
        let bridge_key = ensure_bridge_key(&mut user, BRIDGE_KEY_NAME).await.unwrap();
        let (agent_db, _) = create_agent_db(
            &mut user,
            "chaz",
            &AgentDbConfig::default(),
            &AgentMeta::default(),
        )
        .await
        .unwrap();
        let agent_db_id = agent_db.id();
        let (bridge_db, _) = create_bridge_db(&mut user, "matrix").await.unwrap();
        let settings_db_id = bridge_db.id();

        let err = establish_login(
            &mut user,
            &DeniedBootstrap,
            &BridgeIdentity {
                key: &bridge_key,
                key_name: BRIDGE_KEY_NAME,
            },
            &DatabaseTicket::new(agent_db_id),
            LoginRef {
                kind: "matrix".to_string(),
                identifier: "@chaz:example".to_string(),
                bridge_db_id: settings_db_id.to_string(),
            },
        )
        .await;
        assert!(err.is_err(), "denied access must abort");
        // Nothing was registered.
        assert!(agent_db.list_logins().await.unwrap().is_empty());
    }
}
