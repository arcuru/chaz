//! Worker transcript discovery is not execution or an authorization grant.
use super::{ConversationId, SessionRegistry};
use eidetica::{Database, store::DocStore};

pub fn is_worker_source(source: Option<&str>) -> bool {
    source.is_some_and(|source| source.starts_with("spawn:"))
}

/// Creation kind only. Legacy Workers have no exact display ancestry.
pub async fn is_worker_session(db: &Database) -> anyhow::Result<bool> {
    let txn = db.new_transaction().await?;
    let meta = txn.get_store::<DocStore>("meta").await?.get_all().await?;
    let kind: Option<String> = meta.get("kind").map(TryInto::try_into).transpose()?;
    let name: Option<String> = meta
        .get("display_name")
        .map(TryInto::try_into)
        .transpose()?;
    Ok(kind.as_deref() == Some(crate::db_kind::KIND_SESSION) && is_worker_source(name.as_deref()))
}

impl SessionRegistry {
    /// Explicit observation path for a locally indexed Worker, including one
    /// published after this same-login client's account snapshot was loaded.
    /// Existing bound identities are never replaced by a broader default key.
    pub async fn open_worker_session(
        &self,
        id: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        let row = self
            .list_sessions()
            .await?
            .into_iter()
            .find(|row| row.session_db_id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown Worker session"))?;
        anyhow::ensure!(
            is_worker_source(row.source.as_deref()),
            "not a locally indexed Worker session"
        );
        let root = eidetica::entry::ID::parse(id)?;
        let bound = self.local_client_sessions.lock().await.get(id).cloned();
        let db = if let Some(db) = bound {
            db
        } else {
            let user = self.user.lock().await;
            if let Some(key) = user.find_key(&root)? {
                user.open_database_with_key(&root, &key).await?
            } else {
                // Bind only this observer handle. map_key would overwrite a
                // newer canonical mapping absent from this client's snapshot.
                let key = user.get_default_key()?;
                let signer = user.get_signing_key(&key)?;
                let identity = eidetica::auth::SigKey::from_pubkey(&key);
                let db = if let Some(connection) = self.instance.remote_connection() {
                    // The normal keyed open proves possession to the service's
                    // session keyset; open_remote then validates the same key
                    // against the actual child, without an account-map write.
                    user.open_database_with_key(self.chaz_group.root_id(), &key)
                        .await?;
                    Database::open_remote(
                        &self.instance,
                        connection.clone(),
                        &root,
                        identity.clone(),
                    )
                    .await?
                } else {
                    Database::open(&self.instance, &root).await?
                };
                db.with_key(eidetica::database::DatabaseKey::with_identity(
                    signer, identity,
                ))
            }
        };
        super::jobs::require_observer_write(&db).await?;
        anyhow::ensure!(
            is_worker_session(&db).await?,
            "Worker catalog and session kind disagree"
        );
        self.local_client_sessions
            .lock()
            .await
            .insert(id.to_owned(), db.clone());
        Ok((ConversationId(id.to_owned()), db))
    }

    /// Creation-kind lookup only, never ancestry or authorization evidence.
    pub async fn is_indexed_worker_session(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self
            .list_sessions()
            .await?
            .iter()
            .any(|row| row.session_db_id == id && is_worker_source(row.source.as_deref())))
    }

    /// UI-only opening choice, not a fallback in ordinary `open_session`.
    /// References not already indexed here do not trigger sharing or fetching.
    pub async fn open_session_for_inspection(
        &self,
        id: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        if self.is_indexed_worker_session(id).await? {
            self.open_worker_session(id).await
        } else {
            self.open_session(id).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::AgentRegistry, config::ExecutionRole};
    use eidetica::{
        Instance, NewAccount,
        auth::types::{AuthKey, Permission, SigKey},
        backend::database::InMemory,
        service::ServiceServer,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn worker_observation_repairs_only_missing_same_login_hints_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("workers.sock");
        let (owner, admin) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::with_password("worker-owner", "test-password"),
        )
        .await
        .unwrap();
        admin
            .admin()
            .await
            .unwrap()
            .create_account(NewAccount::with_password("unrelated", "test-password"))
            .await
            .unwrap();
        let service = ServiceServer::bind(owner, &socket).await.unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(());
        let task = tokio::spawn(service.run(receiver));
        let mut settings = crate::config::EideticaConfig {
            connection: format!("unix://{}", socket.display()),
            login: crate::config::EideticaLoginConfig {
                username: "worker-owner".into(),
                password: Some("test-password".into()),
            },
            sync: None,
        };
        let agents = Arc::new(AgentRegistry::with_default_agent());
        let writer = crate::instance::connect_with(&settings, ExecutionRole::Client)
            .await
            .unwrap();
        let writer = SessionRegistry::new(writer.instance, writer.user, agents.clone())
            .await
            .unwrap();
        let (parent_id, parent) = writer.create_session(Some("tui")).await.unwrap();
        let observer = crate::instance::connect_with(&settings, ExecutionRole::Client)
            .await
            .unwrap();
        let observer = SessionRegistry::new(observer.instance, observer.user, agents.clone())
            .await
            .unwrap();
        let mut changes = observer.catalog_changes();
        let mut resident_events = observer.subscribe_new_sessions().await.unwrap();
        let (id, child) = writer
            .create_child_session(&parent_id.0, Some("spawn:positive"))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if resident_events.recv().await.unwrap().session_db_id == id.0 {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let canonical_identity = {
            let mut user = writer.user.lock().await;
            let key = user.get_default_key().unwrap();
            let identity = SigKey::Delegation {
                path: vec![eidetica::auth::types::DelegationStep {
                    tree: parent.root_id().clone(),
                    tips: parent.snapshot().await.unwrap().into_tips(),
                }],
                hint: SigKey::from_pubkey(&key).hint().clone(),
            };
            user.map_key(&key, child.root_id(), identity.clone())
                .await
                .unwrap();
            identity
        };
        assert!(
            observer.open_session(&id.0).await.is_err(),
            "positive control: stale native hint"
        );
        let (_, observed) = observer.open_worker_session(&id.0).await.unwrap();
        assert_eq!(observed.root_id(), child.root_id());
        assert!(is_worker_session(&observed).await.unwrap());
        assert!(observer.open_worker_session(&parent_id.0).await.is_err());
        // A fresh account read, not the writer's old cache, proves observation
        // did not replace the newer canonical delegation with a direct hint.
        let fresh = crate::instance::connect_with(&settings, ExecutionRole::Client)
            .await
            .unwrap();
        assert_eq!(
            fresh
                .user
                .key_mapping(&fresh.user.get_default_key().unwrap(), child.root_id())
                .unwrap(),
            Some(canonical_identity)
        );
        drop(fresh);

        settings.login.username = "unrelated".into();
        let stranger = crate::instance::connect_with(&settings, ExecutionRole::Client)
            .await
            .unwrap();
        let stranger = SessionRegistry::new(stranger.instance, stranger.user, agents)
            .await
            .unwrap();
        assert!(stranger.open_worker_session(&id.0).await.is_err());
        // A forged catalog reference is display data, never an auth grant.
        let txn = stranger.chaz_group.new_transaction().await.unwrap();
        txn.get_store::<DocStore>("sessions")
            .await
            .unwrap()
            .set_string(&id.0, "spawn:forged")
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert!(stranger.open_worker_session(&id.0).await.is_err());

        // An explicit delegated service identity remains unavailable at the
        // native permission-query boundary; never substitute a direct key.
        let delegated = {
            let mut user = stranger.user.lock().await;
            let key = user.get_default_key().unwrap();
            let txn = parent.new_transaction().await.unwrap();
            txn.get_settings()
                .unwrap()
                .set_auth_key(
                    &key,
                    AuthKey::active(Some("delegated observer"), Permission::Write(0)),
                )
                .await
                .unwrap();
            txn.commit().await.unwrap();
            let identity = SigKey::Delegation {
                path: vec![eidetica::auth::types::DelegationStep {
                    tree: parent.root_id().clone(),
                    tips: parent.snapshot().await.unwrap().into_tips(),
                }],
                hint: SigKey::from_pubkey(&key).hint().clone(),
            };
            user.map_key(&key, child.root_id(), identity.clone())
                .await
                .unwrap();
            identity
        };
        let refusal = stranger
            .open_worker_session(&id.0)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("delegated service session identity"),
            "{refusal}"
        );
        assert!(
            stranger
                .local_client_sessions
                .lock()
                .await
                .get(&id.0)
                .is_none()
        );
        {
            let user = stranger.user.lock().await;
            assert_eq!(
                user.key_mapping(&user.get_default_key().unwrap(), child.root_id())
                    .unwrap(),
                Some(delegated)
            );
        }

        // Preserve a deliberately bound narrower identity rather than falling
        // back to the account's still-authorized Admin key.
        let read_db = {
            let mut user = observer.user.lock().await;
            let read_key = user
                .add_private_key(Some("worker-read-only"))
                .await
                .unwrap();
            let txn = child.new_transaction().await.unwrap();
            txn.get_settings()
                .unwrap()
                .set_auth_key(&read_key, AuthKey::active(Some("reader"), Permission::Read))
                .await
                .unwrap();
            txn.commit().await.unwrap();
            user.map_key(&read_key, child.root_id(), SigKey::from_pubkey(&read_key))
                .await
                .unwrap();
            user.open_database_with_key(child.root_id(), &read_key)
                .await
                .unwrap()
        };
        observer
            .local_client_sessions
            .lock()
            .await
            .insert(id.0.clone(), read_db.clone());
        let refusal = observer
            .open_worker_session(&id.0)
            .await
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("Write authority"), "{refusal}");
        assert_eq!(
            observer.local_client_sessions.lock().await[&id.0].auth_identity(),
            read_db.auth_identity()
        );
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
