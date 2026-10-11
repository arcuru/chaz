//! Per-Agent, per-peer durable projection of claimed Agent jobs.
//! The session DB remains authoritative for attempt and terminal receipt.
use crate::agent_db::{AgentDb, ExecutorRef};
use crate::session::jobs::{AcceptedAgentJob, JobState, status_from_db};
use eidetica::Database;
use eidetica::auth::SigKey;
use eidetica::auth::crypto::PublicKey;
use eidetica::auth::types::{
    DelegatedTreeRef, DelegationStep, Permission, PermissionBounds, TreeReference,
};
use eidetica::entry::ID;
use eidetica::store::Table;
use eidetica::user::User;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const JOBS: &str = "jobs";

#[derive(Clone, Debug)]
pub struct ExecutorQueue {
    pub reference: ExecutorRef,
    pub jobs: Vec<ExecutorJob>,
    pub unavailable: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorJob {
    pub session_db_id: String,
    pub parent_session_db_id: String,
    pub agent_db_id: String,
    pub executor_pubkey: String,
    /// Snapshot only. A running row is not a liveness claim; read the session
    /// DB for authoritative attempt/receipt evidence.
    pub last_recorded: JobState,
}

pub async fn list_jobs(db: &Database) -> anyhow::Result<Vec<ExecutorJob>> {
    let txn = db.new_transaction().await?;
    let mut rows: Vec<_> = txn
        .get_store::<Table<ExecutorJob>>(JOBS)
        .await?
        .search(|_: &ExecutorJob| true)
        .await?
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    rows.sort_by(|a, b| a.session_db_id.cmp(&b.session_db_id));
    Ok(rows)
}

/// Idempotent by job ID, with an invariant check on immutable lineage.
pub async fn record(db: &Database, row: ExecutorJob) -> anyhow::Result<()> {
    anyhow::ensure!(
        !matches!(
            row.last_recorded,
            JobState::Pending | JobState::Rejected { .. }
        ),
        "only claimed jobs enter executor DB"
    );
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<Table<ExecutorJob>>(JOBS).await?;
    let existing = store
        .search(|r: &ExecutorJob| r.session_db_id == row.session_db_id)
        .await?;
    anyhow::ensure!(
        existing.iter().all(
            |(_, prior)| prior.parent_session_db_id == row.parent_session_db_id
                && prior.agent_db_id == row.agent_db_id
                && prior.executor_pubkey == row.executor_pubkey
        ),
        "executor job lineage differs"
    );
    if existing.len() == 1
        && (existing[0].1 == row
            || (existing[0].1.last_recorded.is_terminal() && !row.last_recorded.is_terminal()))
    {
        return Ok(());
    }
    for (id, _) in existing {
        store.delete(&id).await?;
    }
    store.insert(row).await?;
    txn.commit().await?;
    Ok(())
}

/// Find before creating: an interrupted creation can leave a DB without its
/// Agent reference. Never create another DB while an orphan with the stable
/// name is present. The peer key owns writes; Agent keyholders inherit Admin
/// through Eidetica delegation, not through copied key material.
pub async fn ensure(
    user: &mut User,
    agent: &AgentDb,
    agent_key: &PublicKey,
) -> anyhow::Result<(Database, PublicKey)> {
    let peer = user.get_default_key()?;
    let name = format!("executor:{}:{peer}", agent.database().root_id());
    // Always select the peer's direct owner key. A client may have mapped
    // the Agent's delegated key on this DB in the shared User key map;
    // find_database chooses an arbitrary mapping, which is unsafe on a
    // remote service and can fail to discover an existing DB.
    let references: Vec<_> = agent
        .list_executors()
        .await?
        .into_iter()
        .filter(|r| r.peer_pubkey == peer.to_string())
        .collect();
    anyhow::ensure!(
        references.len() <= 1,
        "duplicate executor references for peer"
    );
    let mut matches = Vec::new();
    if let Some(reference) = references.first() {
        let db = user
            .open_database_with_key(&ID::parse(&reference.db_id)?, &peer)
            .await?;
        anyhow::ensure!(
            db.get_name().await? == name,
            "executor reference targets another database"
        );
        matches.push(db);
    } else {
        for tracked in user.databases().await? {
            if let Ok(candidate) = user
                .open_database_with_key(&tracked.database_id, &peer)
                .await
                && candidate
                    .get_name()
                    .await
                    .is_ok_and(|candidate_name| candidate_name == name)
            {
                matches.push(candidate);
            }
        }
    }
    anyhow::ensure!(
        matches.len() <= 1,
        "duplicate executor databases for {name}"
    );
    let db = match matches.into_iter().next() {
        Some(db) => db,
        None => {
            let mut settings = eidetica::crdt::Doc::new();
            settings.set("name", name.as_str());
            user.create_database(settings, &peer).await?
        }
    };
    match crate::db_kind::read_marker(&db).await {
        Some((kind, label)) => anyhow::ensure!(
            kind == crate::db_kind::KIND_EXECUTOR && label == name,
            "executor DB marker differs"
        ),
        None => {
            let txn = db.new_transaction().await?;
            txn.get_settings()?
                .add_delegated_tree(DelegatedTreeRef {
                    permission_bounds: PermissionBounds {
                        max: Permission::Admin(0),
                        min: None,
                    },
                    tree: TreeReference {
                        root: agent.database().root_id().clone(),
                        tips: agent.database().snapshot().await?.into_tips(),
                    },
                })
                .await?;
            txn.get_store::<Table<ExecutorJob>>(JOBS).await?;
            txn.commit().await?;
            crate::db_kind::write_marker(&db, crate::db_kind::KIND_EXECUTOR, &name).await?;
        }
    }
    // Do not publish a pointer until a read with the Agent's delegated
    // identity succeeds. Remote find_sigkeys scans through the login key and
    // cannot traverse an Agent DB which authorizes only its own key.
    let reference = ExecutorRef {
        peer_pubkey: peer.to_string(),
        db_id: db.root_id().to_string(),
    };
    let delegated = open_for_agent(user, agent, &reference, agent_key).await?;
    list_jobs(&delegated).await?;
    agent
        .register_executor(ExecutorRef {
            peer_pubkey: peer.to_string(),
            db_id: db.root_id().to_string(),
        })
        .await?;
    Ok((db, peer))
}

pub async fn open_for_agent(
    user: &mut User,
    agent: &AgentDb,
    reference: &ExecutorRef,
    agent_key: &PublicKey,
) -> anyhow::Result<Database> {
    let root = ID::parse(&reference.db_id)?;
    let identity = SigKey::Delegation {
        path: vec![DelegationStep {
            tree: agent.database().root_id().clone(),
            tips: agent.database().snapshot().await?.into_tips(),
        }],
        hint: SigKey::from_pubkey(agent_key).hint().clone(),
    };
    // Mapping selects the Agent key as the remote service identity. The
    // service validates the delegation and denies an unauthorized reference.
    if user.key_mapping(agent_key, &root)? != Some(identity.clone()) {
        user.map_key(agent_key, &root, identity).await?;
    }
    let db = user.open_database_with_key(&root, agent_key).await?;
    // Refuse pointers to another kind of DB even when a key can read it.
    let label = format!(
        "executor:{}:{}",
        agent.database().root_id(),
        reference.peer_pubkey
    );
    anyhow::ensure!(
        crate::db_kind::read_marker(&db)
            .await
            .is_some_and(|(kind, name)| kind == crate::db_kind::KIND_EXECUTOR && name == label),
        "executor reference targets another Agent/executor DB"
    );
    Ok(db)
}

pub(crate) async fn reconcile_job(
    db: &Database,
    session: &Database,
    accepted: &AcceptedAgentJob,
    agent_db_id: &str,
    peer: &str,
    live_attempts: &HashSet<String>,
) -> anyhow::Result<()> {
    let state = status_from_db(session, live_attempts, true).await?.state;
    record(
        db,
        ExecutorJob {
            session_db_id: session.root_id().to_string(),
            parent_session_db_id: accepted.parent_id.clone(),
            agent_db_id: agent_db_id.to_string(),
            executor_pubkey: peer.to_string(),
            last_recorded: state,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};

    #[tokio::test]
    async fn stable_queue_adopts_orphan_and_agent_key_can_read() {
        let (_instance, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("q"))
                .await
                .unwrap();
        let (parent_agent, _) = crate::agent_db::create_agent_db(
            &mut user,
            "A",
            &Default::default(),
            &Default::default(),
        )
        .await
        .unwrap();
        let (agent, key) = crate::agent_db::create_agent_db(
            &mut user,
            "B",
            &Default::default(),
            &Default::default(),
        )
        .await
        .unwrap();
        let peer = user.get_default_key().unwrap();
        let mut settings = eidetica::crdt::Doc::new();
        settings.set(
            "name",
            format!("executor:{}:{peer}", agent.database().root_id()),
        );
        let orphan = user.create_database(settings, &peer).await.unwrap();
        assert!(agent.list_executors().await.unwrap().is_empty());
        let (db, _) = ensure(&mut user, &agent, &key).await.unwrap();
        assert_eq!(db.root_id(), orphan.root_id());
        let (again, _) = ensure(&mut user, &agent, &key).await.unwrap();
        assert_eq!(again.root_id(), db.root_id());
        assert_eq!(agent.list_executors().await.unwrap().len(), 1);
        assert!(parent_agent.list_executors().await.unwrap().is_empty());
        let reference = agent.list_executors().await.unwrap().pop().unwrap();
        let client_view = open_for_agent(&mut user, &agent, &reference, &key)
            .await
            .unwrap();
        assert!(list_jobs(&client_view).await.unwrap().is_empty());
        let row = ExecutorJob {
            session_db_id: "job-1".into(),
            parent_session_db_id: "A".into(),
            agent_db_id: agent.database().root_id().to_string(),
            executor_pubkey: peer.to_string(),
            last_recorded: JobState::Queued,
        };
        record(&db, row.clone()).await.unwrap();
        assert_eq!(list_jobs(&client_view).await.unwrap(), vec![row.clone()]);
        let mut updated = row.clone();
        updated.last_recorded = JobState::Succeeded { text: None };
        record(&db, updated.clone()).await.unwrap();
        assert_eq!(list_jobs(&client_view).await.unwrap(), vec![updated]);
        assert!(
            record(
                &db,
                ExecutorJob {
                    parent_session_db_id: "forged".into(),
                    ..row
                }
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn no_pointer_to_wrong_kind_or_pending_row() {
        let (_instance, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("q"))
                .await
                .unwrap();
        let (agent, key) = crate::agent_db::create_agent_db(
            &mut user,
            "B",
            &Default::default(),
            &Default::default(),
        )
        .await
        .unwrap();
        let peer = user.get_default_key().unwrap();
        let mut settings = eidetica::crdt::Doc::new();
        settings.set(
            "name",
            format!("executor:{}:{peer}", agent.database().root_id()),
        );
        let wrong = user.create_database(settings, &peer).await.unwrap();
        crate::db_kind::write_marker(&wrong, crate::db_kind::KIND_SESSION, "wrong")
            .await
            .unwrap();
        assert!(ensure(&mut user, &agent, &key).await.is_err());
        assert!(agent.list_executors().await.unwrap().is_empty());
        let row = ExecutorJob {
            session_db_id: "job".into(),
            parent_session_db_id: "a".into(),
            agent_db_id: "b".into(),
            executor_pubkey: peer.to_string(),
            last_recorded: JobState::Pending,
        };
        assert!(record(&wrong, row).await.is_err());
    }
}
