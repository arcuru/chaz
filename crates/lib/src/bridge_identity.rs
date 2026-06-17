//! Bridge identity + access bootstrap (core flow; live P2P behind a seam).
//!
//! A standalone bridge runs its **own** eidetica `User` and holds its **own**
//! key — it is not chaz's peer key. This module covers the bridge's
//! agent-facing bring-up:
//!
//! 1. [`ensure_bridge_key`] — self-generate and persist that key on first run,
//!    reuse it thereafter (stable identity across restarts).
//! 2. [`AccessBootstrap`] — the seam over eidetica's ticket-based access
//!    request. The live [`SyncBootstrap`] drives `bootstrap_with_ticket`
//!    against the peer addresses carried in a [`DatabaseTicket`] (the same
//!    path `/agent share` → `/agent import` uses); tests substitute a stub so
//!    the orchestration is exercised without standing up P2P sync. The request
//!    resolves to a [`BootstrapOutcome`]: `Approved` when the bridge's key was
//!    already authorized, `Pending` when the owner must approve via
//!    `/sharing approve` first (the bridge retries later).
//! 3. [`establish_login`] — request `Write` on the owning agent's DB via a
//!    ticket, then (once approved) self-register the public
//!    [`LoginRef`](crate::agent_db::LoginRef) pointer at the bridge settings DB
//!    holding the secret.
//!
//! `Write` (not `Read`) is requested on the agent DB for v1 because the bridge
//! self-registers its own pointer. Read-only-on-AgentDB — chaz writing the
//! pointer on the bridge's behalf — is deferred pending a bootstrap-metadata
//! channel in eidetica (see `logins-in-agent-db` spec + the eidetica friction
//! task). The bridge needs `Write` on session DBs regardless, to proxy-write
//! inbound transport messages.

#![allow(dead_code)]

use crate::agent_db::{AgentDb, LoginRef};
use crate::session::BootstrapOutcome;
use eidetica::auth::crypto::PublicKey;
use eidetica::auth::types::Permission;
use eidetica::sync::{DatabaseTicket, Sync, SyncError};
use eidetica::user::User;
use std::sync::Arc;
use tracing::info;

/// Display name carried by a bridge's own eidetica key — also the
/// `requesting_key_name` used when bootstrapping access.
pub const BRIDGE_KEY_NAME: &str = "bridge";

/// Write priority granted to / requested by a bridge, matching the value chaz
/// uses for agent and bank writes (`Permission::Write(10)`).
const BRIDGE_WRITE_PRIORITY: u32 = 10;

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
/// ([`SyncBootstrap`]) drives the live sync handshake; tests provide a stub so
/// [`establish_login`] can be exercised without P2P sync.
#[allow(async_fn_in_trait)]
pub trait AccessBootstrap {
    /// Request `permission` on the database the `ticket` points at,
    /// authenticating as `(key, key_name)`. Resolves to
    /// [`BootstrapOutcome::Approved`] once access is in hand (the owning peer
    /// had pre-authorized the key and the DB synced locally), or
    /// [`BootstrapOutcome::Pending`] when the owner must approve the queued
    /// request first. Errors only on transport/protocol failure.
    async fn request_access(
        &self,
        ticket: &DatabaseTicket,
        key: &PublicKey,
        key_name: &str,
        permission: Permission,
    ) -> anyhow::Result<BootstrapOutcome>;
}

/// Live `AccessBootstrap` over an eidetica `Sync` handle. Issues a single
/// `bootstrap_with_ticket` (trying every address hint in the ticket); the
/// owner-side approval is the existing chaz `/sharing` flow. Retry-until-
/// approved is the caller/binary's concern (the request stays queued on the
/// owner, and a `Pending` outcome tells the binary to come back later).
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
        ticket: &DatabaseTicket,
        key: &PublicKey,
        key_name: &str,
        permission: Permission,
    ) -> anyhow::Result<BootstrapOutcome> {
        match self
            .sync
            .bootstrap_with_ticket(ticket, key, key_name, permission)
            .await
        {
            Ok(()) => Ok(BootstrapOutcome::Approved),
            Err(e) => {
                if let eidetica::Error::Sync(boxed) = &e
                    && let SyncError::BootstrapPending {
                        request_id,
                        message,
                    } = boxed.as_ref()
                {
                    return Ok(BootstrapOutcome::Pending {
                        request_id: request_id.clone(),
                        message: message.clone(),
                    });
                }
                Err(e.into())
            }
        }
    }
}

/// Identity of the bridge issuing a bootstrap request: its own key plus the
/// display name that key carries (also the `requesting_key_name`).
pub struct BridgeIdentity<'a> {
    pub key: &'a PublicKey,
    pub key_name: &'a str,
}

/// Bring one bridged login online against its owning agent:
///
/// 1. request `Write` on the agent DB the `ticket` points at (via the
///    [`AccessBootstrap`] seam),
/// 2. if the request is still `Pending` owner approval, return that outcome —
///    nothing is registered yet and the binary retries after approval,
/// 3. otherwise open the now-authorized agent DB and self-register the public
///    [`LoginRef`] (`kind` / `identifier` / this bridge's DB id) so peers can
///    discover the login.
///
/// The secret details were already seeded into the bridge's own DB (the
/// bridge manages that); this step only publishes the non-secret pointer.
pub async fn establish_login<B: AccessBootstrap>(
    user: &User,
    bootstrap: &B,
    identity: &BridgeIdentity<'_>,
    ticket: &DatabaseTicket,
    login: LoginRef,
) -> anyhow::Result<BootstrapOutcome> {
    let outcome = bootstrap
        .request_access(
            ticket,
            identity.key,
            identity.key_name,
            Permission::Write(BRIDGE_WRITE_PRIORITY),
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
    let agent_db_id = ticket.database_id();
    let database = user.open_database(agent_db_id).await?;
    let agent_db = AgentDb::from_database(database);
    let identifier = login.identifier.clone();
    agent_db.register_login(login).await?;
    info!(
        agent_db = %agent_db_id,
        login = %identifier,
        "Registered bridge login pointer in agent DB"
    );
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

    /// A no-op bootstrap: access is assumed already in hand. Lets the
    /// orchestration be tested without live sync (the single-user test already
    /// holds admin on the agent DB it created).
    struct GrantedBootstrap;
    impl AccessBootstrap for GrantedBootstrap {
        async fn request_access(
            &self,
            _ticket: &DatabaseTicket,
            _key: &PublicKey,
            _key_name: &str,
            _permission: Permission,
        ) -> anyhow::Result<BootstrapOutcome> {
            Ok(BootstrapOutcome::Approved)
        }
    }

    /// A bootstrap stuck awaiting owner approval — proves `establish_login`
    /// reports `Pending` and registers nothing yet.
    struct PendingBootstrap;
    impl AccessBootstrap for PendingBootstrap {
        async fn request_access(
            &self,
            _ticket: &DatabaseTicket,
            _key: &PublicKey,
            _key_name: &str,
            _permission: Permission,
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
            _ticket: &DatabaseTicket,
            _key: &PublicKey,
            _key_name: &str,
            _permission: Permission,
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
    async fn establish_login_registers_pointer() {
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

        let outcome = establish_login(
            &user,
            &GrantedBootstrap,
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
        let reg = agent_db.find_login("@chaz:example").await.unwrap().unwrap();
        assert_eq!(reg.kind, "matrix");
        assert_eq!(reg.bridge_db_id, settings_db_id.to_string());
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
            &user,
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
            &user,
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
