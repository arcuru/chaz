//! Intentional durable context, separate from disposable request projection.
//!
//! The host invokes a granted contributor once per durable request, then commits
//! its text, strategy state and receipt in one Eidetica transaction. Extensions
//! return data, not database handles or provenance. A retry uses the receipt;
//! a new request gets a new invocation. All namespaces are session/agent bound.

#[cfg(test)]
mod tests;

use super::{ExtensionHub, Scope, caps::CapFuture};
use crate::session::{Session, SessionContextView, TurnRequestId};
use eidetica::{Database, Snapshot, store::Table};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, panic::AssertUnwindSafe, sync::Arc};

pub(crate) const CONTRIBUTIONS_STORE: &str = "context_contributions";
const STATE_STORE: &str = "context_strategy_state";
const MAX_BYTES: usize = 64 * 1024;

/// Explicit operator grant. Installation and declarations do not grant writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableContextGrant {
    pub extension: String,
    #[serde(default)]
    pub required: bool,
}

/// One atomic contribution. Text is untrusted conversation data, never System
/// authority or native tool calls. `state` replaces only this instance's state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextContribution {
    pub messages: Vec<String>,
    pub state: Option<Value>,
}

/// Scoped read-only input; no ambient database/write handle is exposed.
pub struct DurableContextCall<'a> {
    pub invocation_id: &'a str,
    pub request_id: &'a TurnRequestId,
    pub agent_name: &'a str,
    pub session_db_id: &'a str,
    pub scope: Scope,
    /// A coherent selected view, pinned before this contribution.
    pub context: &'a SessionContextView,
    /// Only the calling instance's committed strategy state.
    pub state: Option<&'a Value>,
}

pub trait DurableContextContributor: Send + Sync {
    fn contribute<'a>(
        &'a self,
        call: &'a DurableContextCall<'a>,
    ) -> CapFuture<'a, ContextContribution>;
}

/// Host-assigned identity, persisted with the receipt. Attempt ids deliberately
/// do not participate: retrying an unanswered request must not reinject.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionIdentity {
    pub version: u8,
    pub session_db_id: String,
    pub agent_name: String,
    pub scope: String,
    pub extension: String,
    pub request_id: TurnRequestId,
}

impl ContributionIdentity {
    pub fn namespace(&self) -> String {
        serde_json::to_string(&(
            self.version,
            &self.session_db_id,
            &self.agent_name,
            &self.scope,
            &self.extension,
        ))
        .expect("string tuple serializes")
    }

    pub fn invocation_id(&self) -> String {
        serde_json::to_string(&(self.namespace(), self.request_id.as_str()))
            .expect("string tuple serializes")
    }
}

/// A committed batch is also its dedup receipt, including empty batches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommittedContribution {
    pub identity: ContributionIdentity,
    pub contribution: ContextContribution,
}

impl CommittedContribution {
    pub fn source_id(&self, index: usize) -> String {
        serde_json::to_string(&(self.identity.invocation_id(), index))
            .expect("string tuple serializes")
    }

    fn validate(&self, db: &Database, key: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.identity.version == 1,
            "unsupported contribution version"
        );
        anyhow::ensure!(
            self.identity.session_db_id == db.root_id().to_string()
                && self.identity.invocation_id() == key
                && matches!(self.identity.scope.as_str(), "global" | "agent" | "session")
                && !self.identity.extension.trim().is_empty()
                && !self.identity.agent_name.trim().is_empty(),
            "invalid durable context provenance"
        );
        validate_contribution(&self.contribution)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StrategyState {
    invocation_id: String,
    value: Value,
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Global => "global",
        Scope::PerAgent => "agent",
        Scope::PerSession => "session",
    }
}

fn validate_contribution(contribution: &ContextContribution) -> anyhow::Result<()> {
    anyhow::ensure!(
        contribution.messages.len() <= 64,
        "too many durable messages"
    );
    anyhow::ensure!(
        serde_json::to_vec(contribution)?.len() <= MAX_BYTES,
        "durable contribution exceeds 64 KiB"
    );
    Ok(())
}

pub(crate) async fn read_at(
    db: &Database,
    snapshot: &Snapshot,
) -> anyhow::Result<Vec<CommittedContribution>> {
    let txn = db.new_transaction_at(snapshot).await?;
    let mut rows = txn
        .get_store::<Table<CommittedContribution>>(CONTRIBUTIONS_STORE)
        .await?
        .search(|_| true)
        .await?;
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, row) in &rows {
        row.validate(db, key)?;
    }
    for (namespace, state) in txn
        .get_store::<Table<StrategyState>>(STATE_STORE)
        .await?
        .search(|_| true)
        .await?
    {
        anyhow::ensure!(
            rows.iter().any(|(key, row)| key == &state.invocation_id
                && row.identity.namespace() == namespace
                && row.contribution.state.as_ref() == Some(&state.value)),
            "strategy state lacks a matching committed receipt"
        );
    }
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

/// Shared staging path: dropping or failing this transaction publishes neither
/// the state, text nor the dedup receipt.
async fn stage_contribution(
    txn: &eidetica::transaction::Transaction,
    row: CommittedContribution,
) -> anyhow::Result<()> {
    validate_contribution(&row.contribution)?;
    let invocation_id = row.identity.invocation_id();
    if let Some(value) = &row.contribution.state {
        txn.get_store::<Table<StrategyState>>(STATE_STORE)
            .await?
            .set(
                row.identity.namespace(),
                StrategyState {
                    invocation_id: invocation_id.clone(),
                    value: value.clone(),
                },
            )
            .await?;
    }
    txn.get_store::<Table<CommittedContribution>>(CONTRIBUTIONS_STORE)
        .await?
        .set(invocation_id, row)
        .await?;
    Ok(())
}

struct ResolvedContributor {
    extension: String,
    required: bool,
    scope: Scope,
    endpoint: Arc<dyn DurableContextContributor>,
}

impl ExtensionHub {
    pub fn set_durable_context_grants(
        &mut self,
        grants: &[DurableContextGrant],
    ) -> Result<(), String> {
        let mut seen = HashSet::new();
        for grant in grants {
            if grant.extension.trim().is_empty() || !seen.insert(&grant.extension) {
                return Err("durable_context requires distinct nonempty extension names".into());
            }
        }
        self.durable_context = grants.to_vec();
        Ok(())
    }

    pub fn has_durable_context(&self) -> bool {
        !self.durable_context.is_empty()
    }

    async fn resolve_contributors(
        &self,
        agent: &str,
        db: &Database,
        turn_active: &HashSet<String>,
    ) -> anyhow::Result<Vec<ResolvedContributor>> {
        if self.durable_context.is_empty() {
            return Ok(Vec::new());
        }
        let active = self
            .active_extensions_for_call(agent, Some(db), turn_active)
            .await;
        let instances = self.scoped_instances(agent, Some(db)).await;
        let mut resolved = Vec::new();
        for grant in &self.durable_context {
            let name = &grant.extension;
            let endpoint = instances
                .get(name)
                .filter(|_| active.contains(name))
                .filter(|inst| {
                    inst.manifest()
                        .provides_capabilities
                        .contains(&super::caps::CapabilityKind::DurableContext)
                })
                .and_then(|inst| inst.durable_context_contributor());
            if let Some(endpoint) = endpoint {
                let scope = if self
                    .session_instances
                    .read()
                    .await
                    .contains_key(&(db.root_id().to_string(), name.clone()))
                {
                    Scope::PerSession
                } else if self.agent_instances.read().await.values().any(|inst| {
                    instances
                        .get(name)
                        .is_some_and(|chosen| Arc::ptr_eq(inst, chosen))
                }) {
                    Scope::PerAgent
                } else {
                    Scope::Global
                };
                resolved.push(ResolvedContributor {
                    extension: name.clone(),
                    required: grant.required,
                    scope,
                    endpoint,
                });
            } else if grant.required {
                anyhow::bail!("required durable contributor '{name}' is unavailable or inactive");
            }
        }
        Ok(resolved)
    }

    /// Current grants/activation decide visibility, never persisted tool facts.
    pub(crate) async fn durable_extensions_for_call(
        &self,
        agent: &str,
        db: &Database,
        turn_active: &HashSet<String>,
    ) -> anyhow::Result<HashSet<String>> {
        Ok(self
            .resolve_contributors(agent, db, turn_active)
            .await?
            .into_iter()
            .map(|step| step.extension)
            .collect())
    }

    /// Verify a required contribution was actually committed for this request.
    /// Direct runtime callers must prepare through the host before dispatch too.
    pub(crate) async fn check_durable_receipts(
        &self,
        agent: &str,
        db: &Database,
        request: Option<&TurnRequestId>,
        turn_active: &HashSet<String>,
    ) -> anyhow::Result<()> {
        let steps = self.resolve_contributors(agent, db, turn_active).await?;
        if steps.iter().any(|step| step.required) {
            let request = request.ok_or_else(|| {
                anyhow::anyhow!("required durable context needs a durable request identity")
            })?;
            let rows = read_at(db, &db.snapshot().await?).await?;
            for step in steps.iter().filter(|step| step.required) {
                anyhow::ensure!(
                    rows.iter().any(|row| row.identity.agent_name == agent
                        && row.identity.extension == step.extension
                        && &row.identity.request_id == request
                        && row.identity.scope == scope_name(step.scope)),
                    "required durable contribution '{}' has no committed receipt",
                    step.extension
                );
            }
        }
        Ok(())
    }

    /// Production host operation. Commit before reading the view containing the
    /// contribution. Optional failures discard the entire uncommitted batch;
    /// even fallback must successfully load a fresh coherent committed view.
    pub async fn prepare_durable_context(
        &self,
        session: &Session,
        agent: &str,
        request: Option<&TurnRequestId>,
        turn_active: &HashSet<String>,
    ) -> anyhow::Result<SessionContextView> {
        let db = session.database();
        let steps = self.resolve_contributors(agent, db, turn_active).await?;
        for step in steps {
            let outcome = async {
                let request = request.ok_or_else(|| {
                    anyhow::anyhow!("durable context requires a durable request identity")
                })?;
                let identity = ContributionIdentity {
                    version: 1,
                    session_db_id: db.root_id().to_string(),
                    agent_name: agent.into(),
                    scope: scope_name(step.scope).into(),
                    extension: step.extension.clone(),
                    request_id: request.clone(),
                };
                let invocation_id = identity.invocation_id();
                let snapshot = db.snapshot().await?;
                let view = session.context_view_at(snapshot.clone()).await?;
                if view
                    .contributions
                    .iter()
                    .any(|row| row.identity == identity)
                {
                    return Ok(());
                }
                let txn = db.new_transaction_at(&snapshot).await?;
                let states = txn.get_store::<Table<StrategyState>>(STATE_STORE).await?;
                let state = match states.get(identity.namespace()).await {
                    Ok(state) => Some(state),
                    Err(error) if error.is_not_found() => None,
                    Err(error) => return Err(error.into()),
                };
                // Contributors see only their own durable namespace; transcript
                // access is selected and read-only, not a raw session handle.
                let mut scoped_view = view;
                scoped_view
                    .contributions
                    .retain(|row| row.identity.namespace() == identity.namespace());
                let call = DurableContextCall {
                    invocation_id: &invocation_id,
                    request_id: request,
                    agent_name: agent,
                    session_db_id: &identity.session_db_id,
                    scope: step.scope,
                    context: &scoped_view,
                    state: state.as_ref().map(|state| &state.value),
                };
                let invoked = tokio::time::timeout(
                    super::projection::PROJECTOR_TIMEOUT,
                    AssertUnwindSafe(async { step.endpoint.contribute(&call).await })
                        .catch_unwind(),
                )
                .await;
                let contribution = match invoked {
                    Ok(Ok(result)) => result?,
                    Ok(Err(_)) => anyhow::bail!("contributor panicked"),
                    Err(_) => anyhow::bail!("contributor timed out"),
                };
                // Recheck after awaiting extension code, before any writes.
                anyhow::ensure!(
                    self.durable_extensions_for_call(agent, db, turn_active)
                        .await?
                        .contains(&step.extension),
                    "contribution grant revoked"
                );
                stage_contribution(
                    &txn,
                    CommittedContribution {
                        identity,
                        contribution,
                    },
                )
                .await?;
                txn.commit().await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = outcome {
                if step.required {
                    anyhow::bail!(
                        "required durable contributor '{}' failed: {error}",
                        step.extension
                    );
                }
                tracing::warn!(extension = step.extension, %error, "Optional durable contribution discarded");
            }
        }
        let steps = self.resolve_contributors(agent, db, turn_active).await?;
        let mut view = session.context_view_at(db.snapshot().await?).await?;
        view.contributions.retain(|row| {
            row.identity.agent_name == agent
                && steps.iter().any(|step| {
                    row.identity.extension == step.extension
                        && row.identity.scope == scope_name(step.scope)
                })
        });
        Ok(view)
    }
}
