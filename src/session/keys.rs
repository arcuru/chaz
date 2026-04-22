//! Agent DB helpers + ephemeral key lifecycle. Everything that goes through
//! the registry's `User` to manipulate keys or agent DBs.

use eidetica::auth::types::{AuthKey, Permission};
use tracing::info;

use super::SessionRegistry;

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
    pub async fn open_agent_db(
        &self,
        agent_db_id: &eidetica::entry::ID,
    ) -> anyhow::Result<Option<crate::agent_db::AgentDb>> {
        let user = self.user.lock().await;
        match user.find_key(agent_db_id)? {
            Some(_) => {
                let db = user.open_database(agent_db_id).await?;
                Ok(Some(crate::agent_db::AgentDb::from_database(db)))
            }
            None => Ok(None),
        }
    }

    /// Open a Memory Bank DB via this peer's user (Memory Banks Stage
    /// 9.C). Returns `None` if this peer holds no key for the DB —
    /// expected for bank references an agent's key was revoked from, or
    /// for references we haven't synced yet.
    ///
    /// Known gap: `User::open_database` picks the first key `find_key`
    /// returns rather than the agent's specific key. When the DB is
    /// write-granted to multiple keys on this peer, writes may be signed
    /// by the wrong one. Tracked as the `open_database_with_key` gap in
    /// Eidetica Feedback.
    pub async fn open_memory_bank(
        &self,
        bank_db_id: &eidetica::entry::ID,
    ) -> anyhow::Result<Option<crate::memory_bank_db::MemoryBankDb>> {
        let user = self.user.lock().await;
        match user.find_key(bank_db_id)? {
            Some(_) => {
                let db = user.open_database(bank_db_id).await?;
                Ok(Some(crate::memory_bank_db::MemoryBankDb::from_database(db)))
            }
            None => Ok(None),
        }
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
            .open_memory_bank(bank_db_id)
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
            .open_memory_bank(bank_db_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Peer holds no key for memory bank {bank_db_id}"))?;
        let txn = bank.database().new_transaction().await?;
        let settings = txn.get_settings()?;
        settings.revoke_auth_key(pubkey).await?;
        txn.commit().await?;
        info!(bank_db_id = %bank_db_id, "Revoked key on memory bank");
        Ok(())
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
        let opened = registry.open_agent_db(&fake_id).await.unwrap();
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
