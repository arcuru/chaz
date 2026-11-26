//! Exact direct-key opens for native transcript queries and indexing.

use anyhow::{Context as _, Result, ensure};
use eidetica::Database;
use eidetica::account::AccountSigner;
use eidetica::auth::types::Permission;
use eidetica::entry::ID;
use eidetica::store::{DocStore, Table};

use super::SessionRegistry;
use crate::{
    agent_db::{AgentDb, BankPermission, MEMORY_BANKS_STORE, MemoryBankRef},
    db_kind,
    hosted_index::DbEntry,
};

const UNAVAILABLE: &str = "transcript database unavailable";

impl SessionRegistry {
    /// The caller is the Living Agent selected by the calling turn, not a tool argument.
    pub(crate) async fn open_index_source(
        &self,
        caller: &DbEntry,
        session_id: &str,
    ) -> Result<Database> {
        let open = async {
            let (signer, _) = self.index_actor(caller).await?;
            let root = ID::parse(session_id)?;
            self.open_index_db(&signer, &root, &[db_kind::KIND_SESSION], Permission::Read)
                .await
        };
        open.await.map_err(|_| anyhow::anyhow!(UNAVAILABLE))
    }

    pub(crate) async fn open_index_agent(&self, caller: &DbEntry) -> Result<AgentDb> {
        self.index_actor(caller)
            .await
            .map(|(_, agent_db)| agent_db)
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))
    }

    /// Open a selected memory table as the acting Agent. The selection (attachment,
    /// name, or reference) is a locator, never a grant.
    pub(crate) async fn open_agent_bank(
        &self,
        caller: &DbEntry,
        root: &ID,
        minimum: Permission,
    ) -> Result<Database> {
        let (signer, _) = self.index_actor(caller).await?;
        // Existing Agent-as-bank references can recall the same memory table.
        // Session imports target standalone banks only.
        self.open_index_db(
            &signer,
            root,
            &[db_kind::KIND_BANK, db_kind::KIND_AGENT],
            minimum,
        )
        .await
    }

    pub(crate) async fn open_index_bank(
        &self,
        caller: &DbEntry,
        bank_name: &str,
    ) -> Result<Database> {
        let open = async {
            let (signer, agent_db) = self.index_actor(caller).await?;
            // Aliases aren't table keys. Don't hydrate an unlimited directory
            // or choose the first of two concurrent bindings for the same name.
            let refs = agent_db
                .database()
                .get_store_viewer::<Table<MemoryBankRef>>(MEMORY_BANKS_STORE)
                .await?
                .scan_page(None, 128)
                .await?;
            ensure!(refs.next.is_none(), "bank directory too large");
            let mut matches = refs
                .rows
                .into_iter()
                .map(|(_, bank)| bank)
                .filter(|bank| bank.name == bank_name);
            let bank = matches.next().context("no bank reference")?;
            ensure!(matches.next().is_none(), "ambiguous bank reference");
            ensure!(
                bank.permission == BankPermission::Write,
                "read-only bank reference"
            );
            let root = ID::parse(&bank.db_id)?;
            // The reference is a locator; only native Auth decides whether the agent may write.
            self.open_index_db(
                &signer,
                &root,
                &[db_kind::KIND_BANK],
                Permission::Write(u32::MAX),
            )
            .await
        };
        open.await.map_err(|_| anyhow::anyhow!(UNAVAILABLE))
    }

    async fn index_actor(&self, caller: &DbEntry) -> Result<(AccountSigner, AgentDb)> {
        let signer = self.user.lock().await.acting_as(&caller.pubkey, None)?;
        let db = self
            .open_index_db(
                &signer,
                &caller.db_id,
                &[db_kind::KIND_AGENT],
                Permission::Read,
            )
            .await?;
        Ok((signer, AgentDb::from_database(db)))
    }

    async fn open_index_db(
        &self,
        signer: &AccountSigner,
        root: &ID,
        kinds: &[&str],
        minimum: Permission,
    ) -> Result<Database> {
        // The native explicit signer binds both service reads and commits, without a
        // local target mapping (attach/grant only update target Auth).
        let db = signer.open_database(&self.instance, root).await?;
        Self::check_index_grant(&db, &signer.key_id(), minimum).await?;
        let meta = db.get_store_viewer::<DocStore>(db_kind::META_STORE).await?;
        ensure!(kinds.contains(&meta.get_string(db_kind::KIND_KEY).await?.as_str()));
        Ok(db)
    }

    /// Check the exact active slot at the current frontier. In particular,
    /// a wildcard Read left after revocation is not a grant to this Agent.
    pub(crate) async fn check_index_grant(
        db: &Database,
        key: &eidetica::auth::crypto::PublicKey,
        minimum: Permission,
    ) -> Result<Permission> {
        let grant = db.get_settings().await?.get_auth_key(key).await?;
        ensure!(grant.is_active() && *grant.permissions() >= minimum);
        Ok(*grant.permissions())
    }

    pub(crate) async fn check_index_membership(db: &Database, actor: &DbEntry) -> Result<()> {
        let permission = Self::check_index_grant(db, &actor.pubkey, Permission::Read).await?;
        // In the *invoking* session only a direct Write slot denotes Living
        // Agent participation; an administrative login isn't a participant.
        ensure!(matches!(permission, Permission::Write(_)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_db::{BankPermission, MemoryBankRef};
    use crate::session::test_helpers::{make_agent_entry, make_registry};
    use eidetica::auth::types::{AuthKey, SigKey};

    #[tokio::test]
    async fn index_access_embedded_exact_direct_grants() {
        exercise(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn index_access_service_exact_direct_grants() {
        exercise(true).await;
    }

    async fn exercise(service_mode: bool) {
        let (_instance, owner) = make_registry().await;
        let caller = make_agent_entry(&owner, "reader").await;
        let (source_id, source) = owner.create_session(Some("index-fixture")).await.unwrap();
        let (bank, _) = owner
            .create_new_memory_bank("fixture", &Default::default())
            .await
            .unwrap();
        let agent = owner
            .open_agent_db(&caller.db_id, Some(&caller.pubkey))
            .await
            .unwrap()
            .unwrap();
        #[cfg(unix)]
        let (registry, service) = if service_mode {
            let dir = tempfile::tempdir().unwrap();
            let (shutdown, receiver) = tokio::sync::watch::channel(());
            let socket = dir.path().join("index.sock");
            let service = eidetica::service::ServiceServer::bind(_instance, &socket)
                .await
                .unwrap();
            let task = tokio::spawn(service.run(receiver));
            let client = eidetica::Instance::connect(format!("unix://{}", socket.display()))
                .await
                .unwrap();
            let user = client.login_account("test", "test-password").await.unwrap();
            let registry = SessionRegistry::new(client, user, owner.agents.clone())
                .await
                .unwrap();
            (std::sync::Arc::new(registry), Some((dir, shutdown, task)))
        } else {
            (owner.clone(), None)
        };
        #[cfg(not(unix))]
        let registry = {
            assert!(!service_mode);
            owner.clone()
        };

        let deny = |result: Result<Database>| {
            assert_eq!(format!("{:#}", result.unwrap_err()), UNAVAILABLE);
        };
        // Default/owner DB access does not substitute for a direct grant.
        deny(registry.open_index_source(&caller, &source_id.0).await);
        deny(registry.open_index_source(&caller, "not-an-id").await);
        owner
            .attach_agent_to_session(&source_id.0, &caller)
            .await
            .unwrap();
        let txn = source.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_auth_key(&caller.pubkey, AuthKey::active(None, Permission::Read))
            .await
            .unwrap();
        txn.commit().await.unwrap();

        let reader = registry
            .open_index_source(&caller, &source_id.0)
            .await
            .unwrap();
        let direct = SigKey::from_pubkey(&caller.pubkey);
        assert_eq!(reader.auth_identity(), Some(&direct));
        assert_eq!(reader.current_permission().await.unwrap(), Permission::Read);
        assert_eq!(
            db_kind::read_marker(&reader).await.unwrap().1,
            "index-fixture"
        );
        deny(
            registry
                .open_index_source(&caller, &caller.db_id.to_string())
                .await,
        );

        // Recall preserves Agent-as-bank references but cannot reinterpret a
        // readable session as a bank. Import has a stricter target kind.
        let notes = registry
            .open_agent_bank(&caller, &caller.db_id, Permission::Read)
            .await
            .unwrap();
        assert_eq!(notes.auth_identity(), Some(&direct));
        assert!(
            registry
                .open_agent_bank(&caller, source.root_id(), Permission::Read)
                .await
                .is_err()
        );

        // Even an actual Write grant does not make a bank discoverable by name
        // until the agent DB carries a reference to it.
        owner
            .grant_on_memory_bank(
                &bank.id(),
                &caller.pubkey,
                "fixture-member",
                BankPermission::Write,
            )
            .await
            .unwrap();
        deny(registry.open_index_bank(&caller, "fixture").await);
        agent
            .attach_memory_bank(MemoryBankRef {
                name: "destination".into(),
                db_id: bank.id().to_string(),
                permission: BankPermission::Write,
            })
            .await
            .unwrap();
        let writer = registry
            .open_index_bank(&caller, "destination")
            .await
            .unwrap();
        assert_eq!(writer.auth_identity(), Some(&direct));
        let txn = writer.new_transaction().await.unwrap();
        txn.get_store::<DocStore>("index_probe")
            .await
            .unwrap()
            .set_string("value", "committed by the caller")
            .await
            .unwrap();
        let commit = txn.commit().await.unwrap();
        let signed = bank.database().get_entry(commit.clone()).await.unwrap();
        assert_eq!(signed.auth().key, direct);
        assert!(
            bank.database()
                .verify_entry_signature(commit)
                .await
                .unwrap()
        );

        let user = registry.user.lock().await;
        for root in [source.root_id(), &bank.id()] {
            assert_eq!(user.key_mapping(&caller.pubkey, root).unwrap(), None);
        }
        drop(user);

        owner
            .grant_on_memory_bank(
                &bank.id(),
                &caller.pubkey,
                "fixture-member",
                BankPermission::Read,
            )
            .await
            .unwrap();
        // Stale advertised Write is not authority.
        deny(registry.open_index_bank(&caller, "destination").await);

        let txn = source.new_transaction().await.unwrap();
        let settings = txn.get_settings().unwrap();
        settings.revoke_auth_key(&caller.pubkey).await.unwrap();
        settings
            .set_global_auth_key(AuthKey::active(None, Permission::Read))
            .await
            .unwrap();
        txn.commit().await.unwrap();
        // A retained native handle now has wildcard Read, and attachment left
        // delegation in place; neither qualifies for a fresh index open.
        assert_eq!(reader.current_permission().await.unwrap(), Permission::Read);
        deny(registry.open_index_source(&caller, &source_id.0).await);

        let wrong_agent = DbEntry {
            db_id: bank.id(),
            ..caller.clone()
        };
        assert!(registry.open_index_agent(&wrong_agent).await.is_err());

        #[cfg(unix)]
        if let Some((_dir, shutdown, task)) = service {
            shutdown.send(()).unwrap();
            task.await.unwrap().unwrap();
        }
    }
}
