//! Workflow parents are parked job-shaped sessions. Catalog rows and client
//! writes are discovery hints, never authority to execute a graph node.
use super::{SessionIndex, SessionRegistry};
use crate::extensions::orchestrator::spec::FlowSpec;
use crate::grants::Grants;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const INTENTS: &str = "workflow_intents";
const ACCEPTANCE: &str = "workflow_acceptance";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowAuthority {
    pub agent: String,
    pub agent_db_id: String,
    pub home_pubkey: String,
    pub call_depth: usize,
    pub max_call_depth: usize,
    pub tool_ceilings: BTreeMap<String, Grants>,
    pub capability_ceiling: Grants,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcceptedWorkflowParent {
    pub parent_id: String,
    pub request_key_hash: String,
    pub flow: FlowSpec,
    pub authority: WorkflowAuthority,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum WorkflowParentState {
    Staged,
    Accepted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkflowParentStatus {
    pub session_db_id: String,
    pub state: WorkflowParentState,
}

fn hash(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex().to_string()
}

pub(crate) fn source(parent: &str, key: &str) -> String {
    format!("workflow-stage:{}", hash(&format!("{parent}\0{key}")))
}

pub(crate) async fn is_workflow_parent(db: &eidetica::Database) -> bool {
    crate::db_kind::read_marker(db)
        .await
        .is_some_and(|(kind, name)| {
            kind == crate::db_kind::KIND_SESSION && name.starts_with("workflow-stage:")
        })
}

async fn read_json<T: serde::de::DeserializeOwned>(
    db: &eidetica::Database,
    store: &str,
    key: &str,
) -> anyhow::Result<Option<T>> {
    let txn = db.new_transaction().await?;
    let doc = txn.get_store::<DocStore>(store).await?.get_all().await?;
    match doc.get(key) {
        Some(value) => {
            let json: String = value.try_into()?;
            Ok(Some(serde_json::from_str(&json)?))
        }
        None => Ok(None),
    }
}

pub(crate) async fn read_accepted_workflow(
    db: &eidetica::Database,
) -> anyhow::Result<Option<AcceptedWorkflowParent>> {
    read_json(db, ACCEPTANCE, "v1").await
}

fn matching_rows<'a>(rows: &'a [SessionIndex], label: &str) -> Vec<&'a SessionIndex> {
    rows.iter()
        .filter(|row| row.source.as_deref() == Some(label))
        .collect()
}

impl SessionRegistry {
    /// The executor holds its command-admission lock and has already checked
    /// the caller, graph and ceiling. A failed cross-DB write is uncertain.
    /// No catalog row or partial child is promoted on retry by inference.
    pub(crate) async fn accept_workflow_parent(
        &self,
        parent_id: &str,
        request_key: &str,
        flow: FlowSpec,
        authority: WorkflowAuthority,
        agent: &crate::hosted_index::DbEntry,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(!request_key.is_empty(), "workflow request key required");
        let expected = AcceptedWorkflowParent {
            parent_id: parent_id.to_string(),
            request_key_hash: hash(request_key),
            flow,
            authority,
        };
        let (_, parent) = self.open_session(parent_id).await?;
        let key = hash(request_key);
        if let Some(prior) = read_json::<AcceptedWorkflowParent>(&parent, INTENTS, &key).await? {
            anyhow::ensure!(
                prior == expected,
                "workflow request key reused with a different definition"
            );
        }
        let label = source(parent_id, request_key);
        let rows = self.list_sessions().await?;
        let matches = matching_rows(&rows, &label);
        anyhow::ensure!(
            matches.len() <= 1,
            "uncertain workflow submission: multiple catalog parents"
        );
        if let Some(row) = matches.first() {
            let (_, db) = self.open_job_session(&row.session_db_id).await?;
            anyhow::ensure!(
                read_accepted_workflow(&db).await? == Some(expected),
                "uncertain workflow submission: partial or mismatched parent"
            );
            return Ok(row.session_db_id.clone());
        }
        anyhow::ensure!(
            read_json::<AcceptedWorkflowParent>(&parent, INTENTS, &key)
                .await?
                .is_none(),
            "uncertain workflow submission: intent without catalog parent"
        );
        let txn = parent.new_transaction().await?;
        txn.get_store::<DocStore>(INTENTS)
            .await?
            .set_string(key, serde_json::to_string(&expected)?)
            .await?;
        txn.commit()
            .await
            .map_err(|e| anyhow::anyhow!("uncertain workflow submission: {e}"))?;
        let (id, db) = self
            .create_child_session(parent_id, Some(&label))
            .await
            .map_err(|e| anyhow::anyhow!("uncertain workflow submission: {e}"))?;
        self.attach_agent_to_session(&id.0, agent)
            .await
            .map_err(|e| anyhow::anyhow!("uncertain workflow submission: {e}"))?;
        // Metadata is a cache of the ceiling; the accepted record is committed
        // with the complete graph in one child DB transaction. No Directive.
        let child = super::Session::new(id.clone(), db.clone()).await;
        child
            .update_meta(|meta| meta.capabilities = expected.authority.capability_ceiling.clone())
            .await
            .map_err(|e| anyhow::anyhow!("uncertain workflow submission: {e}"))?;
        let txn = db.new_transaction().await?;
        txn.get_store::<DocStore>(ACCEPTANCE)
            .await?
            .set_string("v1", serde_json::to_string(&expected)?)
            .await?;
        txn.commit()
            .await
            .map_err(|e| anyhow::anyhow!("uncertain workflow submission: {e}"))?;
        Ok(id.0)
    }

    pub async fn lookup_workflow_parent(
        &self,
        parent_id: &str,
        request_key: &str,
    ) -> anyhow::Result<Option<String>> {
        anyhow::ensure!(!request_key.is_empty(), "workflow request key required");
        let (_, parent) = self.open_session(parent_id).await?;
        let intent: Option<AcceptedWorkflowParent> =
            read_json(&parent, INTENTS, &hash(request_key)).await?;
        let rows = self.list_sessions().await?;
        let matches = matching_rows(&rows, &source(parent_id, request_key));
        anyhow::ensure!(
            matches.len() <= 1,
            "uncertain workflow request: multiple parents"
        );
        let Some(row) = matches.first() else {
            anyhow::ensure!(
                intent.is_none(),
                "uncertain workflow request: intent without parent"
            );
            return Ok(None);
        };
        let (_, db) = self.open_job_session(&row.session_db_id).await?;
        anyhow::ensure!(
            intent.is_some() && read_accepted_workflow(&db).await? == intent,
            "uncertain workflow request: partial or mismatched parent"
        );
        Ok(Some(row.session_db_id.clone()))
    }
}
