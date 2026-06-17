//! Bridge identity + access bootstrap (core flow; live P2P behind a seam).
//!
//! A standalone bridge runs its **own** eidetica `User` and holds its **own**
//! key — it is not chaz's peer key. This module covers the bridge's
//! agent-facing bring-up:
//!
//! 1. [`ensure_bridge_key`] — self-generate and persist that key on first run,
//!    reuse it thereafter (stable identity across restarts).
//! 2. [`AccessBootstrap`] — the seam over eidetica's access-request flow. The
//!    live [`SyncBootstrap`] drives `sync_with_peer_for_bootstrap_with_key`
//!    against a peer address; tests substitute a stub so the orchestration is
//!    exercised without standing up P2P sync.
//! 3. [`establish_login`] — request `Write` on the owning agent's DB, then
//!    self-register the public [`LoginRef`](crate::agent_db::LoginRef) pointer
//!    at the bridge settings DB holding the secret.
//!
//! `Write` (not `Read`) is requested on the agent DB for v1 because the bridge
//! self-registers its own pointer. Read-only-on-AgentDB — chaz writing the
//! pointer on the bridge's behalf — is deferred pending a bootstrap-metadata
//! channel in eidetica (see `logins-in-agent-db` spec + the eidetica friction
//! task). The bridge needs `Write` on session DBs regardless, to proxy-write
//! inbound transport messages.

#![allow(dead_code)]

use crate::agent_db::{AgentDb, LoginRef};
use crate::bridge_db::LoginCredentials;
use eidetica::auth::crypto::PublicKey;
use eidetica::auth::types::Permission;
use eidetica::entry::ID;
use eidetica::sync::{Address, Sync};
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

/// Seam over eidetica's access-request bootstrap. The real implementation
/// ([`SyncBootstrap`]) drives the live sync/peer handshake; tests provide a
/// stub so [`establish_login`] can be exercised without P2P sync.
#[allow(async_fn_in_trait)]
pub trait AccessBootstrap {
    /// Request `permission` on the database `tree_id`, authenticating as
    /// `(key, key_name)`. Resolves once access is in hand (the owning peer
    /// has approved and the DB has synced locally) or errors on failure.
    async fn request_access(
        &self,
        tree_id: &ID,
        key: &PublicKey,
        key_name: &str,
        permission: Permission,
    ) -> anyhow::Result<()>;
}

/// Live `AccessBootstrap` over an eidetica `Sync` handle + a peer address.
/// Issues a single `sync_with_peer_for_bootstrap_with_key`; the owner-side
/// approval is the existing chaz `/sharing` flow. Retry-until-approved is the
/// caller/binary's concern (the request stays queued on the owner).
pub struct SyncBootstrap {
    pub sync: Arc<Sync>,
    pub peer: Address,
}

impl SyncBootstrap {
    pub fn new(sync: Arc<Sync>, peer: Address) -> Self {
        Self { sync, peer }
    }
}

impl AccessBootstrap for SyncBootstrap {
    async fn request_access(
        &self,
        tree_id: &ID,
        key: &PublicKey,
        key_name: &str,
        permission: Permission,
    ) -> anyhow::Result<()> {
        self.sync
            .sync_with_peer_for_bootstrap_with_key(&self.peer, tree_id, key, key_name, permission)
            .await?;
        Ok(())
    }
}

/// Build the public [`LoginRef`] for a credential set — pointing at the bridge
/// settings DB that holds the secret — and register it in the agent's DB.
/// Idempotent (upsert by identifier), so a bridge re-registering after a
/// settings-DB change updates the pointer in place.
pub async fn register_login_pointer(
    agent_db: &AgentDb,
    creds: &LoginCredentials,
    bridge_settings_db_id: &ID,
) -> anyhow::Result<()> {
    agent_db
        .register_login(LoginRef {
            kind: creds.kind.clone(),
            identifier: creds.identifier.clone(),
            bridge_db_id: bridge_settings_db_id.to_string(),
        })
        .await
}

/// Bring one bridged login online against its owning agent:
///
/// 1. request `Write` on the agent DB (via the [`AccessBootstrap`] seam),
/// 2. open the now-authorized agent DB,
/// 3. self-register the public [`LoginRef`] pointing at the bridge settings DB.
///
/// The credentials themselves were already seeded into the bridge settings DB
/// (see [`crate::bridge_config::BridgeConfig::seed_into`]); this step only
/// publishes the non-secret pointer so peers can discover the login.
pub async fn establish_login<B: AccessBootstrap>(
    user: &User,
    bootstrap: &B,
    bridge_key: &PublicKey,
    bridge_key_name: &str,
    agent_db_id: &ID,
    bridge_settings_db_id: &ID,
    creds: &LoginCredentials,
) -> anyhow::Result<()> {
    bootstrap
        .request_access(
            agent_db_id,
            bridge_key,
            bridge_key_name,
            Permission::Write(BRIDGE_WRITE_PRIORITY),
        )
        .await?;
    let database = user.open_database(agent_db_id).await?;
    let agent_db = AgentDb::from_database(database);
    register_login_pointer(&agent_db, creds, bridge_settings_db_id).await?;
    info!(
        agent_db = %agent_db_id,
        login = %creds.identifier,
        "Registered bridge login pointer in agent DB"
    );
    Ok(())
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

    fn matrix_creds(identifier: &str) -> LoginCredentials {
        LoginCredentials {
            kind: "matrix".to_string(),
            identifier: identifier.to_string(),
            homeserver_url: Some("https://matrix.example".to_string()),
            username: Some(identifier.to_string()),
            secret: Some("hunter2".to_string()),
            allow_list: None,
            room_size_limit: None,
        }
    }

    /// A no-op bootstrap: access is assumed already in hand. Lets the
    /// orchestration be tested without live sync (the single-user test already
    /// holds admin on the agent DB it created).
    struct GrantedBootstrap;
    impl AccessBootstrap for GrantedBootstrap {
        async fn request_access(
            &self,
            _tree_id: &ID,
            _key: &PublicKey,
            _key_name: &str,
            _permission: Permission,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A bootstrap that always fails — proves `establish_login` aborts before
    /// touching the agent DB when access can't be obtained.
    struct DeniedBootstrap;
    impl AccessBootstrap for DeniedBootstrap {
        async fn request_access(
            &self,
            _tree_id: &ID,
            _key: &PublicKey,
            _key_name: &str,
            _permission: Permission,
        ) -> anyhow::Result<()> {
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
        let creds = matrix_creds("@chaz:example");

        establish_login(
            &user,
            &GrantedBootstrap,
            &bridge_key,
            BRIDGE_KEY_NAME,
            &agent_db_id,
            &settings_db_id,
            &creds,
        )
        .await
        .unwrap();

        let reg = agent_db.find_login("@chaz:example").await.unwrap().unwrap();
        assert_eq!(reg.kind, "matrix");
        assert_eq!(reg.bridge_db_id, settings_db_id.to_string());
        // The registry pointer carries no secret.
        let json = serde_json::to_string(&reg).unwrap();
        assert!(!json.contains("hunter2"));
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
            &bridge_key,
            BRIDGE_KEY_NAME,
            &agent_db_id,
            &settings_db_id,
            &matrix_creds("@chaz:example"),
        )
        .await;
        assert!(err.is_err(), "denied access must abort");
        // Nothing was registered.
        assert!(agent_db.list_logins().await.unwrap().is_empty());
    }
}
