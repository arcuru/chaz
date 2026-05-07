//! Agent DB helpers + ephemeral key lifecycle. Everything that goes through
//! the registry's `User` to manipulate keys or agent DBs.

use eidetica::auth::types::{AuthKey, Permission};
use eidetica::sync::{BootstrapRequest, DatabaseTicket, SyncError};
use tracing::info;

use super::SessionRegistry;

/// Outcome of a bootstrap request initiated via `request_db_access`.
/// Eidetica's `bootstrap_with_ticket` either approves immediately (when the
/// requester's pubkey is already authorized on the target DB) or queues a
/// `BootstrapPending` request for an Admin to approve.
#[derive(Debug, Clone)]
pub enum BootstrapOutcome {
    /// Sync proceeded — the requester's pubkey was already preseeded (or the
    /// DB is open to the requested permission). Caller can immediately open
    /// the DB and register it locally.
    Approved,
    /// Owner must approve via `/sharing approve <request_id>`. Until then the
    /// requester sees no entries on this DB and cannot open it. The receiver
    /// must re-run the request after approval to actually pull the entries.
    Pending { request_id: String, message: String },
}

impl SessionRegistry {
    /// Create a new Agent DB for the Living Agents lifecycle (Stage 6
    /// `/agent new`). Wraps `agent_db::create_agent_db` with the registry's
    /// user mutex and rejects duplicate display names up front.
    pub async fn create_new_agent_db(
        &self,
        display_name: &str,
        cfg: &crate::agent_db::AgentDbConfig,
        meta: &crate::agent_db::AgentMeta,
    ) -> anyhow::Result<(crate::agent_db::AgentDb, eidetica::auth::crypto::PublicKey)> {
        let mut user = self.user.lock().await;
        if let Some((existing, _)) = crate::agent_db::find_agent_db(&user, display_name).await {
            anyhow::bail!(
                "Agent '{}' already exists (DB {})",
                display_name,
                existing.id()
            );
        }
        crate::agent_db::create_agent_db(&mut user, display_name, cfg, meta).await
    }

    /// Open an Agent DB via this peer's user. Succeeds only if this peer
    /// holds a key for the DB (e.g. an agent it created, or one synced and
    /// then key-shared). Returns `None` if the user doesn't hold a key.
    ///
    /// `pubkey` selects which user-held key signs subsequent writes; pass
    /// `Some` when the caller knows the pubkey (e.g. from `HostedIndex`)
    /// to avoid `find_key`'s arbitrary first-match. Pass `None` to fall
    /// back to `find_key` for callers without a specific key in mind.
    pub async fn open_agent_db(
        &self,
        agent_db_id: &eidetica::entry::ID,
        pubkey: Option<&eidetica::auth::crypto::PublicKey>,
    ) -> anyhow::Result<Option<crate::agent_db::AgentDb>> {
        let user = self.user.lock().await;
        let key = match pubkey {
            Some(pk) => pk.clone(),
            None => match user.find_key(agent_db_id)? {
                Some(k) => k,
                None => return Ok(None),
            },
        };
        let db = user.open_database_with_key(agent_db_id, &key).await?;
        Ok(Some(crate::agent_db::AgentDb::from_database(db)))
    }

    /// Open a Memory Bank DB via this peer's user (Memory Banks Stage
    /// 9.C). Returns `None` if this peer holds no key for the DB —
    /// expected for bank references an agent's key was revoked from, or
    /// for references we haven't synced yet.
    ///
    /// `pubkey` selects which user-held key signs subsequent writes; pass
    /// `Some` when the caller knows the pubkey (e.g. an agent writing to
    /// a bank it has been granted access to). Pass `None` to fall back to
    /// `find_key` for ambient lookups.
    pub async fn open_memory_bank(
        &self,
        bank_db_id: &eidetica::entry::ID,
        pubkey: Option<&eidetica::auth::crypto::PublicKey>,
    ) -> anyhow::Result<Option<crate::memory_bank_db::MemoryBankDb>> {
        let user = self.user.lock().await;
        let key = match pubkey {
            Some(pk) => pk.clone(),
            None => match user.find_key(bank_db_id)? {
                Some(k) => k,
                None => return Ok(None),
            },
        };
        let db = user.open_database_with_key(bank_db_id, &key).await?;
        Ok(Some(crate::memory_bank_db::MemoryBankDb::from_database(db)))
    }

    /// Create a Memory Bank DB for the `/memory new` command (Stage
    /// 9.D). Wraps `memory_bank_db::create_memory_bank` with the
    /// registry's user mutex; rejects duplicate display names so
    /// peer-local names stay unique.
    pub async fn create_new_memory_bank(
        &self,
        display_name: &str,
        meta: &crate::memory_bank_db::MemoryBankMeta,
    ) -> anyhow::Result<(
        crate::memory_bank_db::MemoryBankDb,
        eidetica::auth::crypto::PublicKey,
    )> {
        let mut user = self.user.lock().await;
        if let Some((existing, _)) =
            crate::memory_bank_db::find_memory_bank(&user, display_name).await
        {
            anyhow::bail!(
                "Memory bank '{}' already exists (DB {})",
                display_name,
                existing.id()
            );
        }
        crate::memory_bank_db::create_memory_bank(&mut user, display_name, meta).await
    }

    /// Test-only: lock the internal user mutex and return a guard. Lets
    /// fixtures call `create_agent_db` directly without duplicating the
    /// user-mutex plumbing.
    #[cfg(test)]
    pub async fn user_for_tests(&self) -> tokio::sync::MutexGuard<'_, eidetica::user::User> {
        self.user.lock().await
    }

    /// Lock the internal user mutex and return a guard. Used by
    /// `hosted_index::build_from_user` at startup to walk
    /// `User::databases()`. Outside that path, prefer the more focused
    /// helpers (`open_agent_db`, `find_key_for_db`, etc.) so the lock scope
    /// stays narrow and well-known.
    pub async fn user_lock(&self) -> tokio::sync::MutexGuard<'_, eidetica::user::User> {
        self.user.lock().await
    }

    /// Look up the first pubkey this peer holds for a database. Thin wrapper
    /// around `User::find_key` so callers don't need direct access to the
    /// user mutex.
    pub async fn find_key_for_db(
        &self,
        db_id: &eidetica::entry::ID,
    ) -> anyhow::Result<Option<eidetica::auth::crypto::PublicKey>> {
        let user = self.user.lock().await;
        Ok(user.find_key(db_id)?)
    }

    /// Generate a fresh ephemeral keypair on this peer's `User` and return the
    /// pubkey. Used by Living Agents Stage 5 `spawn_task` for one-shot runs:
    /// the new key is authorized on a child session, runs the task, then
    /// revoked. The DB retains the revoked key's signatures as an audit
    /// record but no new writes can be made under that key.
    pub async fn new_ephemeral_key(
        &self,
        label: &str,
    ) -> anyhow::Result<eidetica::auth::crypto::PublicKey> {
        let mut user = self.user.lock().await;
        let pubkey = user.add_private_key(Some(label)).await?;
        Ok(pubkey)
    }

    /// Authorize a pubkey with `Permission::Write(power)` on a session. Used
    /// by `spawn_task` to grant the ephemeral key write access to its child
    /// session before the ReAct loop runs.
    pub async fn grant_write_on_session(
        &self,
        session_db_id: &str,
        pubkey: &eidetica::auth::crypto::PublicKey,
        key_label: &str,
        power: u32,
    ) -> anyhow::Result<()> {
        let (_conv, session_db) = self.open_session(session_db_id).await?;
        let txn = session_db.new_transaction().await?;
        let settings = txn.get_settings()?;
        settings
            .set_auth_key(
                pubkey,
                AuthKey::active(Some(key_label), Permission::Write(power)),
            )
            .await?;
        txn.commit().await?;
        info!(session_db_id, key_label, power, "Granted Write on session");
        Ok(())
    }

    /// Authorize a pubkey on a Memory Bank DB (Stage 9.D.2). Used by
    /// `/memory grant` to give an agent's key `Read` or `Write` on the
    /// bank's AuthSettings. `power` matters for `Write` (higher power
    /// can revoke lower); `Read` ignores it.
    pub async fn grant_on_memory_bank(
        &self,
        bank_db_id: &eidetica::entry::ID,
        pubkey: &eidetica::auth::crypto::PublicKey,
        key_label: &str,
        permission: crate::agent_db::BankPermission,
    ) -> anyhow::Result<()> {
        let bank = self
            .open_memory_bank(bank_db_id, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Peer holds no key for memory bank {bank_db_id}"))?;
        let eidetica_perm = match permission {
            crate::agent_db::BankPermission::Read => Permission::Read,
            crate::agent_db::BankPermission::Write => Permission::Write(10),
        };
        let txn = bank.database().new_transaction().await?;
        let settings = txn.get_settings()?;
        settings
            .set_auth_key(pubkey, AuthKey::active(Some(key_label), eidetica_perm))
            .await?;
        txn.commit().await?;
        info!(
            bank_db_id = %bank_db_id,
            key_label,
            permission = ?permission,
            "Granted key on memory bank"
        );
        Ok(())
    }

    /// Revoke a pubkey on a Memory Bank DB (Stage 9.D.2). Used by
    /// `/memory revoke` to withdraw an agent's access. Historical
    /// entries signed by the key remain verifiable; no new writes.
    pub async fn revoke_on_memory_bank(
        &self,
        bank_db_id: &eidetica::entry::ID,
        pubkey: &eidetica::auth::crypto::PublicKey,
    ) -> anyhow::Result<()> {
        let bank = self
            .open_memory_bank(bank_db_id, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Peer holds no key for memory bank {bank_db_id}"))?;
        let txn = bank.database().new_transaction().await?;
        let settings = txn.get_settings()?;
        settings.revoke_auth_key(pubkey).await?;
        txn.commit().await?;
        info!(bank_db_id = %bank_db_id, "Revoked key on memory bank");
        Ok(())
    }

    /// Authorize a pubkey on an Agent DB (Co-owned Agents Stage 10).
    /// Used by `/agent invite` to give another peer's pubkey admin/write/
    /// read permission on the agent DB's AuthSettings. Fails if this peer
    /// holds no key for the agent — you can't invite someone to an agent
    /// you don't own.
    pub async fn grant_on_agent_db(
        &self,
        agent_db_id: &eidetica::entry::ID,
        pubkey: &eidetica::auth::crypto::PublicKey,
        key_label: &str,
        permission: Permission,
    ) -> anyhow::Result<()> {
        let agent_db = self
            .open_agent_db(agent_db_id, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Peer holds no key for agent DB {agent_db_id}"))?;
        let txn = agent_db.database().new_transaction().await?;
        let settings = txn.get_settings()?;
        settings
            .set_auth_key(pubkey, AuthKey::active(Some(key_label), permission))
            .await?;
        txn.commit().await?;
        info!(
            agent_db_id = %agent_db_id,
            key_label,
            permission = ?permission,
            "Granted key on agent DB"
        );
        Ok(())
    }

    /// Revoke a pubkey on an Agent DB (Co-owned Agents Stage 10).
    /// Used by `/agent revoke-peer`. Historical entries signed by the key
    /// remain verifiable; no new writes.
    pub async fn revoke_on_agent_db(
        &self,
        agent_db_id: &eidetica::entry::ID,
        pubkey: &eidetica::auth::crypto::PublicKey,
    ) -> anyhow::Result<()> {
        let agent_db = self
            .open_agent_db(agent_db_id, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Peer holds no key for agent DB {agent_db_id}"))?;
        let txn = agent_db.database().new_transaction().await?;
        let settings = txn.get_settings()?;
        settings.revoke_auth_key(pubkey).await?;
        txn.commit().await?;
        info!(agent_db_id = %agent_db_id, "Revoked key on agent DB");
        Ok(())
    }

    /// Return this peer's default pubkey — used by `/pubkey` so an agent
    /// owner can paste it into `/agent invite` on another peer.
    pub async fn default_pubkey(&self) -> anyhow::Result<eidetica::auth::crypto::PublicKey> {
        let user = self.user.lock().await;
        Ok(user.get_default_key()?)
    }

    /// Flip `sync_enabled = true` on the user's `TrackedDatabase` entry for
    /// `db_id`, so the source peer actually serves the DB to ticket holders
    /// (and the receiver picks up subsequent writes after import).
    ///
    /// `User::create_database` and `sync_with_ticket` both default tracked
    /// DBs to `SyncSettings::disabled()`. Without this flip, `/share`
    /// produces a valid-looking ticket but the source peer refuses to serve,
    /// and imported DBs go stale after the initial fetch.
    ///
    /// Idempotent — eidetica's `User::enable_sync` short-circuits if the bit
    /// is already on, and errors with `DatabaseNotTracked` for DBs this user
    /// doesn't track (i.e. doesn't hold a key for).
    pub async fn enable_sync_for(&self, db_id: &eidetica::entry::ID) -> anyhow::Result<()> {
        let mut user = self.user.lock().await;
        user.enable_sync(db_id).await?;
        info!(db_id = %db_id, "Enabled sync for DB");
        Ok(())
    }

    /// Atomically enable sync for `db_id` and build a `DatabaseTicket` with
    /// this peer's transport addresses populated. Wraps eidetica's
    /// `User::share`, which combines `enable_sync` + `Sync::create_ticket`
    /// so the track → enable → ticket-build sequence stays in one place.
    ///
    /// Used by `/agent share`, `/memory share`, and `/session share` to
    /// produce a ticket the holder can paste into the corresponding import
    /// command on another peer.
    pub async fn share_for(
        &self,
        db_id: &eidetica::entry::ID,
    ) -> anyhow::Result<DatabaseTicket> {
        let mut user = self.user.lock().await;
        let ticket = user.share(db_id).await?;
        info!(db_id = %db_id, "Shared DB (sync enabled, ticket built)");
        Ok(ticket)
    }

    /// Inverse of `enable_sync_for`. Used by future "stop sharing this DB"
    /// flows. Idempotent — eidetica short-circuits if already off.
    pub async fn disable_sync_for(&self, db_id: &eidetica::entry::ID) -> anyhow::Result<()> {
        let mut user = self.user.lock().await;
        user.disable_sync(db_id).await?;
        info!(db_id = %db_id, "Disabled sync for DB");
        Ok(())
    }

    /// Whether this peer's user has sync enabled for `db_id`. Returns
    /// `Ok(false)` for DBs the user doesn't track. Useful for tests and
    /// future status commands.
    pub async fn is_sync_enabled_for(&self, db_id: &eidetica::entry::ID) -> anyhow::Result<bool> {
        let user = self.user.lock().await;
        Ok(user.is_sync_enabled(db_id).await?)
    }

    /// Request access to a remote DB via eidetica's bootstrap workflow. The
    /// receiver's default pubkey is sent to the source peer along with the
    /// requested permission; the source peer either approves automatically
    /// (key already authorized) or queues a pending request.
    ///
    /// On `Approved`, the caller should open the DB and finish chaz-side
    /// registration (read meta, populate hosted index, upsert runtime
    /// agent, enable sync). On `Pending`, instruct the user to re-run the
    /// request after the owner approves — eidetica doesn't push back to us.
    pub async fn request_db_access(
        &self,
        ticket: &DatabaseTicket,
        permission: Permission,
    ) -> anyhow::Result<BootstrapOutcome> {
        let sync = self
            .instance
            .sync()
            .ok_or_else(|| anyhow::anyhow!("Sync not enabled"))?;
        let user = self.user.lock().await;
        let key_id = user.get_default_key()?;
        match user
            .request_database_access(&sync, ticket, &key_id, permission)
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

    /// List all bootstrap requests on this peer's `_sync` DB that are still
    /// `Pending`. Each entry carries `tree_id`, requester pubkey, requested
    /// permission, and timestamp — enough for `/sharing requests` to render
    /// a queue. Resource names (agent/bank/session) are resolved by the
    /// caller via the hosted indices since eidetica's request only stores
    /// the DB id.
    pub async fn pending_bootstrap_requests(
        &self,
    ) -> anyhow::Result<Vec<(String, BootstrapRequest)>> {
        let sync = self
            .instance
            .sync()
            .ok_or_else(|| anyhow::anyhow!("Sync not enabled"))?;
        let user = self.user.lock().await;
        Ok(user.pending_bootstrap_requests(&sync).await?)
    }

    /// Approve a queued bootstrap request. The requested permission is
    /// granted as-is; if the owner wants a different permission, they must
    /// reject and use `/agent invite` (preseed) instead. Returns the target
    /// DB id so the caller can render a confirmation message.
    ///
    /// Errors if this peer holds no key with Admin on the target DB —
    /// only owners (Admin(0)) and co-admins (Admin(1)) can approve.
    pub async fn approve_bootstrap_request(
        &self,
        request_id: &str,
    ) -> anyhow::Result<(eidetica::entry::ID, BootstrapRequest)> {
        let sync = self
            .instance
            .sync()
            .ok_or_else(|| anyhow::anyhow!("Sync not enabled"))?;
        let req = sync
            .get_bootstrap_request(request_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("No bootstrap request with id '{request_id}'"))?
            .1;
        let user = self.user.lock().await;
        let approving_key = user.find_key(&req.tree_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "This peer holds no key for DB {} — only an admin on that DB can approve",
                req.tree_id
            )
        })?;
        user.approve_bootstrap_request(&sync, request_id, &approving_key)
            .await?;
        info!(
            request_id,
            tree_id = %req.tree_id,
            permission = ?req.requested_permission,
            "Approved bootstrap request"
        );
        Ok((req.tree_id.clone(), req))
    }

    /// Reject a queued bootstrap request. Same admin-key requirement as
    /// `approve_bootstrap_request`. The request is marked rejected; the
    /// requester's bootstrap retry will keep failing until rejected
    /// requests roll off (no eidetica TTL today — see PLAN.md).
    pub async fn reject_bootstrap_request(
        &self,
        request_id: &str,
    ) -> anyhow::Result<(eidetica::entry::ID, BootstrapRequest)> {
        let sync = self
            .instance
            .sync()
            .ok_or_else(|| anyhow::anyhow!("Sync not enabled"))?;
        let req = sync
            .get_bootstrap_request(request_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("No bootstrap request with id '{request_id}'"))?
            .1;
        let user = self.user.lock().await;
        let rejecting_key = user.find_key(&req.tree_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "This peer holds no key for DB {} — only an admin on that DB can reject",
                req.tree_id
            )
        })?;
        user.reject_bootstrap_request(&sync, request_id, &rejecting_key)
            .await?;
        info!(
            request_id,
            tree_id = %req.tree_id,
            "Rejected bootstrap request"
        );
        Ok((req.tree_id.clone(), req))
    }

    /// Revoke a pubkey on a session. Used by `spawn_task` after the task
    /// completes — historical entries signed by the key remain verifiable,
    /// but no new entries can be written.
    pub async fn revoke_key_on_session(
        &self,
        session_db_id: &str,
        pubkey: &eidetica::auth::crypto::PublicKey,
    ) -> anyhow::Result<()> {
        let (_conv, session_db) = self.open_session(session_db_id).await?;
        let txn = session_db.new_transaction().await?;
        let settings = txn.get_settings()?;
        settings.revoke_auth_key(pubkey).await?;
        txn.commit().await?;
        info!(session_db_id, "Revoked auth key on session");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;

    #[tokio::test]
    async fn create_new_agent_db_registers_key_and_rejects_duplicates() {
        let (_instance, registry) = make_registry().await;
        let cfg = crate::agent_db::AgentDbConfig::default();
        let meta = crate::agent_db::AgentMeta {
            display_name: Some("gamma".to_string()),
            ..Default::default()
        };

        let (db, pubkey) = registry
            .create_new_agent_db("gamma", &cfg, &meta)
            .await
            .unwrap();
        assert_eq!(
            registry.find_key_for_db(&db.id()).await.unwrap(),
            Some(pubkey)
        );

        // Re-creating with the same display name is rejected (by DB name match).
        let result = registry.create_new_agent_db("gamma", &cfg, &meta).await;
        let msg = result
            .err()
            .map(|e| format!("{e}"))
            .expect("expected duplicate-name rejection");
        assert!(msg.contains("already exists"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn open_agent_db_returns_none_without_key() {
        let (_instance, registry) = make_registry().await;

        // Fabricate a well-formed but unowned DB ID: `ID::parse` will accept
        // any valid-looking hash, and `find_key` returns None for databases
        // the user doesn't own.
        let fake_id = eidetica::entry::ID::parse(
            "bafyreibog4r3zw5d53sv5u72tms2vz5ye5eudvmqc7tfpn2bjfyebqtqzm",
        )
        .unwrap();
        let opened = registry.open_agent_db(&fake_id, None).await.unwrap();
        assert!(opened.is_none());
    }

    #[tokio::test]
    async fn ephemeral_key_lifecycle_on_session() {
        let (_instance, registry) = make_registry().await;
        let (_conv, session_db) = registry.create_session(Some("task")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        // Generate + authorize ephemeral key.
        let pubkey = registry.new_ephemeral_key("task:test").await.unwrap();
        registry
            .grant_write_on_session(&session_id, &pubkey, "task:test", 100)
            .await
            .unwrap();

        let settings = session_db.get_settings().await.unwrap();
        let auth = settings.get_auth_key(&pubkey).await.unwrap();
        assert_eq!(auth.permissions(), &Permission::Write(100));
        assert_eq!(auth.status(), &eidetica::auth::types::KeyStatus::Active);

        // Revoke, confirm the key is no longer active.
        registry
            .revoke_key_on_session(&session_id, &pubkey)
            .await
            .unwrap();
        let auth_after = session_db
            .get_settings()
            .await
            .unwrap()
            .get_auth_key(&pubkey)
            .await
            .unwrap();
        assert_ne!(
            auth_after.status(),
            &eidetica::auth::types::KeyStatus::Active
        );
    }

    #[tokio::test]
    async fn default_pubkey_returns_users_key() {
        let (_instance, registry) = make_registry().await;
        let pk = registry.default_pubkey().await.unwrap();
        // Round-trip the prefixed representation to prove it's a real key.
        let as_str = pk.to_prefixed_string();
        assert!(as_str.starts_with("ed25519:"), "got {as_str}");
    }

    #[tokio::test]
    async fn grant_and_revoke_on_agent_db_lifecycle() {
        let (_instance, registry) = make_registry().await;
        let cfg = crate::agent_db::AgentDbConfig::default();
        let meta = crate::agent_db::AgentMeta {
            display_name: Some("alpha".to_string()),
            ..Default::default()
        };
        let (agent_db, _owner_pk) = registry
            .create_new_agent_db("alpha", &cfg, &meta)
            .await
            .unwrap();
        let db_id = agent_db.id();

        // Synthesize a second pubkey — the "remote peer" we'd be inviting.
        let invitee_pk = registry.new_ephemeral_key("invitee:test").await.unwrap();

        registry
            .grant_on_agent_db(&db_id, &invitee_pk, "co-admin:test", Permission::Admin(1))
            .await
            .unwrap();
        let auth = agent_db
            .database()
            .get_settings()
            .await
            .unwrap()
            .get_auth_key(&invitee_pk)
            .await
            .unwrap();
        assert_eq!(auth.permissions(), &Permission::Admin(1));
        assert_eq!(auth.status(), &eidetica::auth::types::KeyStatus::Active);

        registry
            .revoke_on_agent_db(&db_id, &invitee_pk)
            .await
            .unwrap();
        let auth_after = agent_db
            .database()
            .get_settings()
            .await
            .unwrap()
            .get_auth_key(&invitee_pk)
            .await
            .unwrap();
        assert_ne!(
            auth_after.status(),
            &eidetica::auth::types::KeyStatus::Active
        );
    }

    #[tokio::test]
    async fn enable_sync_for_flips_tracked_database() {
        let (_instance, registry) = make_registry().await;
        let cfg = crate::agent_db::AgentDbConfig::default();
        let meta = crate::agent_db::AgentMeta {
            display_name: Some("syncable".to_string()),
            ..Default::default()
        };
        let (agent_db, _pk) = registry
            .create_new_agent_db("syncable", &cfg, &meta)
            .await
            .unwrap();
        let db_id = agent_db.id();

        // Fresh user.create_database lands sync_enabled = false.
        assert!(!registry.is_sync_enabled_for(&db_id).await.unwrap());

        registry.enable_sync_for(&db_id).await.unwrap();
        assert!(registry.is_sync_enabled_for(&db_id).await.unwrap());

        // Idempotent — second call is a no-op.
        registry.enable_sync_for(&db_id).await.unwrap();
        assert!(registry.is_sync_enabled_for(&db_id).await.unwrap());
    }

    #[tokio::test]
    async fn enable_sync_for_errors_on_untracked_db() {
        let (_instance, registry) = make_registry().await;
        let fake_id = eidetica::entry::ID::parse(
            "bafyreibog4r3zw5d53sv5u72tms2vz5ye5eudvmqc7tfpn2bjfyebqtqzm",
        )
        .unwrap();
        let err = registry
            .enable_sync_for(&fake_id)
            .await
            .expect_err("expected error for untracked DB");
        assert!(
            err.to_string().contains("not tracked")
                || err.to_string().to_lowercase().contains("not tracked"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn pending_bootstrap_requests_empty_on_fresh_peer() {
        let (_instance, registry) = make_registry_with_sync().await;
        let pending = registry.pending_bootstrap_requests().await.unwrap();
        assert!(pending.is_empty(), "fresh peer should have no requests");
    }

    #[tokio::test]
    async fn bootstrap_helpers_error_when_sync_not_enabled() {
        let (_instance, registry) = make_registry().await;
        let err = registry
            .pending_bootstrap_requests()
            .await
            .expect_err("expected error without sync");
        assert!(
            err.to_string().contains("Sync not enabled"),
            "unexpected error: {err}"
        );

        let err = registry
            .approve_bootstrap_request("does-not-matter")
            .await
            .expect_err("expected error without sync");
        assert!(
            err.to_string().contains("Sync not enabled"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn approve_unknown_request_errors() {
        let (_instance, registry) = make_registry_with_sync().await;
        let err = registry
            .approve_bootstrap_request("not-a-real-id")
            .await
            .expect_err("expected error for unknown id");
        assert!(
            err.to_string().contains("No bootstrap request"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn reject_unknown_request_errors() {
        let (_instance, registry) = make_registry_with_sync().await;
        let err = registry
            .reject_bootstrap_request("not-a-real-id")
            .await
            .expect_err("expected error for unknown id");
        assert!(
            err.to_string().contains("No bootstrap request"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn revoked_ephemeral_key_does_not_block_session_reads() {
        // Regression check: revoking the task key must not brick the session DB.
        // After revoke, the parent-admin key (user default) can still commit.
        let (_instance, registry) = make_registry().await;
        let (_parent_conv, parent_db) = registry.create_session(Some("parent")).await.unwrap();
        let parent_id = parent_db.root_id().to_string();
        let (_child_conv, child_db) = registry
            .create_child_session(&parent_id, Some("task"))
            .await
            .unwrap();
        let child_id = child_db.root_id().to_string();

        let pubkey = registry.new_ephemeral_key("task:test").await.unwrap();
        registry
            .grant_write_on_session(&child_id, &pubkey, "task:test", 100)
            .await
            .unwrap();
        registry
            .revoke_key_on_session(&child_id, &pubkey)
            .await
            .unwrap();

        // Can still read settings post-revoke.
        let auth_after = child_db
            .get_settings()
            .await
            .unwrap()
            .get_auth_key(&pubkey)
            .await
            .unwrap();
        assert_ne!(
            auth_after.status(),
            &eidetica::auth::types::KeyStatus::Active
        );
    }
}
