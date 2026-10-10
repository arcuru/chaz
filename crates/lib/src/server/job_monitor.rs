//! Non-executing local monitor with existing Write authority (Admin allowed).
//! Projections discover claimed jobs; session DBs supply authority/evidence.
//! No refresh here registers a runtime or syncs peers.
use super::Server;
use crate::session::{
    Session, SessionEntry,
    jobs::{JobState, JobWait},
};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Serialize)]
pub struct JobMonitorNode {
    pub session_db_id: String,
    pub parent_id: Option<String>,
    pub agent: String,
    pub executor: String,
    pub claimed: bool,
    pub state: Option<JobState>,
    pub unavailable: Option<String>,
    pub task: String,
    pub waits: Vec<JobWait>,
    pub preview: Vec<SessionEntry>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct JobMonitor {
    pub nodes: Vec<JobMonitorNode>,
    pub sources: Vec<String>,
    pub observed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Server {
    pub async fn job_monitor(&self) -> anyhow::Result<JobMonitor> {
        let mut monitor = JobMonitor::default();
        let mut nodes = BTreeMap::new();
        for agent in self.agent_index.list() {
            let queues = match self
                .registry
                .agent_executor_queues(&agent.db_id, &agent.pubkey)
                .await
            {
                Ok(queues) => queues,
                Err(error) => {
                    monitor
                        .sources
                        .push(format!("{}: unavailable ({error})", agent.display_name));
                    continue;
                }
            };
            for queue in queues {
                monitor.sources.push(format!(
                    "{} / {}: {}",
                    agent.display_name,
                    queue.reference.peer_pubkey,
                    queue
                        .unavailable
                        .as_deref()
                        .map_or("local service snapshot".into(), |e| format!(
                            "unavailable ({e}); remote fetch is not performed"
                        ))
                ));
                for row in queue.jobs {
                    let mut node = self.read_monitor_node(&row.session_db_id).await;
                    node.agent = agent.display_name.clone();
                    node.executor = row.executor_pubkey;
                    node.claimed = true;
                    // Unreadable canonical data remains a visible gap. A row
                    // can label recorded ancestry but cannot authorize work.
                    if node.unavailable.is_some() {
                        node.parent_id = Some(row.parent_session_db_id);
                        node.state = Some(row.last_recorded);
                    }
                    nodes.insert(node.session_db_id.clone(), node);
                }
            }
        }
        let mut references: Vec<String> = nodes
            .values()
            .flat_map(|node| {
                node.parent_id
                    .iter()
                    .cloned()
                    .chain(node.waits.iter().map(|wait| wait.child_id.clone()))
            })
            .collect();
        while let Some(id) = references.pop() {
            if nodes.contains_key(&id) {
                continue;
            }
            let node = self.read_monitor_node(&id).await;
            references.extend(node.parent_id.iter().cloned());
            nodes.insert(id, node);
        }
        let mut order: Vec<_> = nodes
            .keys()
            .map(|id| {
                let mut path = vec![id.clone()];
                let mut parent = nodes[id].parent_id.as_ref();
                let mut seen = HashSet::new();
                while let Some(id) = parent {
                    if !seen.insert(id) {
                        break;
                    }
                    path.push(id.clone());
                    parent = nodes.get(id).and_then(|node| node.parent_id.as_ref());
                }
                path.reverse();
                (path, id.clone())
            })
            .collect();
        order.sort();
        monitor.nodes = order
            .into_iter()
            .filter_map(|(_, id)| nodes.remove(&id))
            .collect();
        monitor.observed_at = Some(chrono::Utc::now());
        Ok(monitor)
    }

    async fn read_monitor_node(&self, id: &str) -> JobMonitorNode {
        let mut node = JobMonitorNode {
            session_db_id: id.into(),
            parent_id: None,
            agent: "context / unclaimed reference".into(),
            executor: String::new(),
            claimed: false,
            state: None,
            unavailable: None,
            task: String::new(),
            waits: Vec::new(),
            preview: Vec::new(),
        };
        let read = async {
            let (cid, db) = match self.registry.open_session(id).await {
                Ok(opened) => opened,
                Err(_) => self.registry.open_job_session(id).await?,
            };
            crate::session::jobs::require_observer_write(&db).await?;
            let session = Session::new(cid, db.clone()).await;
            node.preview = session.entries().iter().rev().take(4).cloned().collect();
            node.preview.reverse();
            if crate::session::jobs::is_job_session(&db).await {
                let request = crate::session::SessionRegistry::read_job_request(&db).await?;
                node.parent_id = Some(request.parent_id);
                node.task = request.task;
                node.state = Some(
                    crate::session::jobs::status_from_db(&db, &Default::default(), false)
                        .await?
                        .state,
                );
                node.waits = crate::session::jobs::job_waits(&db).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = read {
            node.unavailable = Some(error.to_string());
        }
        node
    }
}
