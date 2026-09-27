//! Non-runnable job admission staging. No agent turn may consume a staged row.
//!
//! Eidetica commits one database at a time. The parent intent is written first;
//! child creation publishes a catalog row before delegation and job metadata
//! can be committed. A retry may recover a fully staged child, but must hold
//! a catalog row without metadata as uncertain rather than creating another.

use super::{SessionIndex, SessionRegistry};
use crate::grants::Grants;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};

const INTENTS: &str = "job_intents";
const DEFINITION: &str = "job_definition";

/// Trusted-local broad-scope only. Workspace and private jobs are not admitted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StagedAgentDefinition {
    pub target: String,
    pub task: String,
    pub executor_pubkey: String,
    pub call_depth: usize,
    pub max_call_depth: usize,
    /// Concrete tool names, captured before any turn can run (not glob patterns).
    pub allowed_tools: Vec<String>,
    pub capability_ceiling: Grants,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Intent {
    definition: StagedAgentDefinition,
    child_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ChildDefinition {
    parent_id: String,
    request_hash: String,
    definition: StagedAgentDefinition,
}

/// A staged handle is NOT an accepted runnable job. Its DB has no Directive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedJob {
    pub session_db_id: String,
}

fn source(parent: &str, key: &str) -> String {
    // A catalog label is discovery only, not authority. Never put task text or
    // the caller's request key into this peer-local catalog.
    format!(
        "job-stage:{}",
        blake3::hash(format!("{parent}\0{key}").as_bytes()).to_hex()
    )
}

pub(crate) async fn is_staged_job(db: &eidetica::Database) -> bool {
    crate::db_kind::read_marker(db)
        .await
        .is_some_and(|(kind, name)| {
            kind == crate::db_kind::KIND_SESSION && name.starts_with("job-stage:")
        })
}

fn request_hash(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex().to_string()
}

async fn read_intent(parent: &eidetica::Database, key: &str) -> anyhow::Result<Option<Intent>> {
    let txn = parent.new_transaction().await?;
    let store = txn.get_store::<DocStore>(INTENTS).await?;
    let doc = store.get_all().await?;
    match doc.get(request_hash(key)) {
        Some(value) => {
            let json: String = value.try_into()?;
            Ok(Some(serde_json::from_str(&json)?))
        }
        None => Ok(None),
    }
}

async fn write_intent(
    parent: &eidetica::Database,
    key: &str,
    intent: &Intent,
) -> anyhow::Result<()> {
    let txn = parent.new_transaction().await?;
    txn.get_store::<DocStore>(INTENTS)
        .await?
        .set_string(request_hash(key), serde_json::to_string(intent)?)
        .await?;
    txn.commit().await?;
    Ok(())
}

async fn read_child(db: &eidetica::Database) -> anyhow::Result<Option<ChildDefinition>> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(DEFINITION).await?;
    let doc = store.get_all().await?;
    match doc.get("v1") {
        Some(value) => {
            let json: String = value.try_into()?;
            Ok(Some(serde_json::from_str(&json)?))
        }
        None => Ok(None),
    }
}

fn matching_rows<'a>(rows: &'a [SessionIndex], label: &str) -> Vec<&'a SessionIndex> {
    rows.iter()
        .filter(|row| row.source.as_deref() == Some(label))
        .collect()
}

impl SessionRegistry {
    /// Stage a definition without writing a runnable Directive. The caller's
    /// stable key is scoped to the parent session. A pending intent whose child
    /// cannot be proven complete is *uncertain* and requires reconciliation;
    /// it is never replaced by another child. This API is not job acceptance.
    ///
    /// Single-executor staging only: this serializes competing callers in
    /// one registry; it is not a cross-process or cross-peer fence.
    pub async fn stage_agent_job(
        &self,
        parent_id: &str,
        request_key: &str,
        definition: StagedAgentDefinition,
    ) -> anyhow::Result<StagedJob> {
        anyhow::ensure!(!request_key.is_empty(), "job request key must not be empty");
        // coding: global lock; use per-key locks if staging throughput matters.
        let _guard = self.job_stage_lock.lock().await;
        let label = source(parent_id, request_key);
        anyhow::ensure!(
            !definition.target.is_empty() && !definition.task.is_empty(),
            "job target and task are required"
        );
        anyhow::ensure!(
            !definition.executor_pubkey.is_empty(),
            "designated executor is required"
        );
        anyhow::ensure!(
            definition.call_depth <= definition.max_call_depth,
            "spawn depth exceeds ceiling"
        );
        let (_, parent) = self.open_session(parent_id).await?;
        let prior = read_intent(&parent, request_key).await?;
        if let Some(ref intent) = prior {
            anyhow::ensure!(
                intent.definition == definition,
                "job request key reused with a different definition"
            );
        }
        // A catalog row may be visible before delegation/metadata exists. Do
        // not trust it as a job or repair an incomplete child by inference.
        let rows = self.list_sessions().await?;
        let matches = matching_rows(&rows, &label);
        anyhow::ensure!(
            matches.len() <= 1,
            "uncertain job submission: multiple catalog children"
        );
        if let Some(row) = matches.first() {
            anyhow::ensure!(
                prior.is_some(),
                "uncertain job submission: child has no parent intent"
            );
            let (_, child) = self.open_session(&row.session_db_id).await?;
            anyhow::ensure!(
                read_child(&child).await?
                    == Some(ChildDefinition {
                        parent_id: parent_id.to_string(),
                        request_hash: request_hash(request_key),
                        definition: definition.clone()
                    }),
                "uncertain job submission: child has no matching definition"
            );
            if prior.as_ref().and_then(|i| i.child_id.as_deref()).is_none() {
                write_intent(
                    &parent,
                    request_key,
                    &Intent {
                        definition,
                        child_id: Some(row.session_db_id.clone()),
                    },
                )
                .await?;
            } else {
                anyhow::ensure!(
                    prior.as_ref().and_then(|i| i.child_id.as_deref())
                        == Some(row.session_db_id.as_str()),
                    "uncertain job submission: parent and catalog disagree"
                );
            }
            return Ok(StagedJob {
                session_db_id: row.session_db_id.clone(),
            });
        }
        anyhow::ensure!(
            prior.is_none(),
            "uncertain job submission: intent exists but child is not discoverable"
        );
        // A failed intent commit may have succeeded; return uncertainty rather
        // than treating an error as proof the key is free.
        write_intent(
            &parent,
            request_key,
            &Intent {
                definition: definition.clone(),
                child_id: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("uncertain job submission for request key: {e}"))?;
        let (id, child) = self
            .create_child_session(parent_id, Some(&label))
            .await
            .map_err(|e| anyhow::anyhow!("uncertain job submission for request key: {e}"))?;
        let txn = child.new_transaction().await?;
        txn.get_store::<DocStore>(DEFINITION)
            .await?
            .set_string(
                "v1",
                serde_json::to_string(&ChildDefinition {
                    parent_id: parent_id.to_string(),
                    request_hash: request_hash(request_key),
                    definition: definition.clone(),
                })?,
            )
            .await?;
        txn.commit()
            .await
            .map_err(|e| anyhow::anyhow!("uncertain job submission for request key: {e}"))?;
        write_intent(
            &parent,
            request_key,
            &Intent {
                definition,
                child_id: Some(id.0.clone()),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("uncertain job submission for request key: {e}"))?;
        Ok(StagedJob {
            session_db_id: id.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, test_helpers::make_registry};

    fn definition() -> StagedAgentDefinition {
        StagedAgentDefinition {
            target: "default".into(),
            task: "hello".into(),
            executor_pubkey: "ed25519:local".into(),
            call_depth: 1,
            max_call_depth: 3,
            allowed_tools: vec!["calculate".into()],
            capability_ceiling: Grants::default(),
        }
    }

    #[tokio::test]
    async fn competing_submitters_share_one_staged_handle() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let results = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            futures::future::join_all((0..24).map(|_| {
                let registry = registry.clone();
                let pid = pid.clone();
                async move {
                    registry
                        .stage_agent_job(&pid, "same.key", definition())
                        .await
                }
            })),
        )
        .await
        .expect("concurrent staging must terminate");
        let ids: std::collections::HashSet<_> = results
            .into_iter()
            .map(|result| result.unwrap().session_db_id)
            .collect();
        assert_eq!(ids.len(), 1);
        assert_eq!(
            matching_rows(
                &registry.list_sessions().await.unwrap(),
                &source(&pid, "same.key")
            )
            .len(),
            1
        );
        let id = ids.into_iter().next().unwrap();
        let (id, child) = registry.open_session(&id).await.unwrap();
        assert!(Session::new(id, child).await.entries().is_empty());
    }

    #[tokio::test]
    async fn competing_mismatched_definitions_never_create_two_children() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let mut other = definition();
        other.capability_ceiling = Grants {
            shell: Some(crate::grants::ShellGrant {
                allow: crate::grants::Allowlist::Only(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (a, b) = tokio::join!(
            registry.stage_agent_job(&pid, "collision", definition()),
            registry.stage_agent_job(&pid, "collision", other)
        );
        assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1);
        let error = a.err().or_else(|| b.err()).unwrap();
        assert!(error.to_string().contains("different definition"));
        assert_eq!(
            matching_rows(
                &registry.list_sessions().await.unwrap(),
                &source(&pid, "collision")
            )
            .len(),
            1
        );
    }

    #[tokio::test]
    async fn staged_job_is_inert_and_lost_ack_retry_returns_same_child() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let first = registry
            .stage_agent_job(&pid, "stable.key", definition())
            .await
            .unwrap();
        let second = registry
            .stage_agent_job(&pid, "stable.key", definition())
            .await
            .unwrap();
        assert_eq!(first, second);
        let (id, child) = registry.open_session(&first.session_db_id).await.unwrap();
        assert!(Session::new(id, child.clone()).await.entries().is_empty());
        assert!(is_staged_job(&child).await);
        assert_eq!(
            read_child(&child).await.unwrap().unwrap().definition,
            definition()
        );
        let mut changed = definition();
        changed.task = "different".into();
        assert!(
            registry
                .stage_agent_job(&pid, "stable.key", changed)
                .await
                .unwrap_err()
                .to_string()
                .contains("different definition")
        );
        assert_eq!(
            matching_rows(
                &registry.list_sessions().await.unwrap(),
                &source(&pid, "stable.key")
            )
            .len(),
            1
        );
    }

    #[tokio::test]
    async fn partial_creation_is_held_without_duplicate() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        write_intent(
            &parent,
            "key",
            &Intent {
                definition: definition(),
                child_id: None,
            },
        )
        .await
        .unwrap();
        let (_, partial) = registry
            .create_child_session(&pid, Some(&source(&pid, "key")))
            .await
            .unwrap();
        let err = registry
            .stage_agent_job(&pid, "key", definition())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("uncertain"));
        assert_eq!(
            matching_rows(
                &registry.list_sessions().await.unwrap(),
                &source(&pid, "key")
            )
            .len(),
            1
        );
        assert!(read_child(&partial).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn recover_child_after_parent_link_write_is_lost() {
        let (_, registry) = make_registry().await;
        let (_, parent) = registry.create_session(Some("cli")).await.unwrap();
        let pid = parent.root_id().to_string();
        let key = "lost.ack";
        write_intent(
            &parent,
            key,
            &Intent {
                definition: definition(),
                child_id: None,
            },
        )
        .await
        .unwrap();
        let (_, child) = registry
            .create_child_session(&pid, Some(&source(&pid, key)))
            .await
            .unwrap();
        let child_id = child.root_id().to_string();
        let txn = child.new_transaction().await.unwrap();
        txn.get_store::<DocStore>(DEFINITION)
            .await
            .unwrap()
            .set_string(
                "v1",
                serde_json::to_string(&ChildDefinition {
                    parent_id: pid.clone(),
                    request_hash: request_hash(key),
                    definition: definition(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let staged = registry
            .stage_agent_job(&pid, key, definition())
            .await
            .unwrap();
        assert_eq!(staged.session_db_id, child_id);
        assert_eq!(
            read_intent(&parent, key)
                .await
                .unwrap()
                .unwrap()
                .child_id
                .as_deref(),
            Some(child_id.as_str())
        );
    }
}
