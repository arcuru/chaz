//! Scoped `AgentStateAdmin` implementation — the hub's factory for
//! building per-extension agent-state handles from the raw infra handles
//! (`HostedIndex` + `SessionRegistry`) narrowed to the operator's
//! configured agent allowlist.
//!
//! This is a **guardrail, not a sandbox** — the scope check is a
//! defensive check against poorly behaved tools, not a security boundary
//! against adversarial code.

use std::collections::HashSet;
use std::sync::Arc;

use crate::agent_db::AgentDb;
use crate::extension::caps::{AgentStateAdmin, CapFuture};
use crate::hosted_index::{DbEntry, HostedIndex};
use crate::session::SessionRegistry;

/// An `AgentStateAdmin` whose `resolve_agent` and `open_agent_db` reject
/// agents outside an operator-configured allowlist.
///
/// The hub constructs one per extension that declares `AgentStateAdmin`
/// in its manifest, scoped to the set of agent names the operator allows
/// for that extension in `tool_policy`.
pub struct ScopedAgentStateAdmin {
    registry: Arc<SessionRegistry>,
    index: HostedIndex,
    /// `Some(set)` — only agents in `set` are accessible (even if
    /// `set` is empty, which means deny-all). `None` — unrestricted;
    /// all hosted agents are visible.
    allowed: Option<HashSet<String>>,
}

impl ScopedAgentStateAdmin {
    /// Build a scoped handle for the given agent allowlist. When
    /// `allowlist` is `None`, all hosted agents are visible (the
    /// operator hasn't applied a narrowing yet). When `allowlist` is
    /// `Some(empty)`, every operation returns `Err` — the cap was
    /// effectively denied.
    pub fn new(
        registry: Arc<SessionRegistry>,
        index: HostedIndex,
        allowlist: Option<Vec<String>>,
    ) -> Self {
        let allowed = allowlist.map(|list| list.into_iter().collect());
        Self {
            registry,
            index,
            allowed,
        }
    }

    /// `true` when `allowlist` was `None` — the operator didn't apply
    /// any agent-level scoping. Useful for diagnostics
    /// (`/extensions list -v`).
    #[allow(dead_code)]
    pub fn is_unrestricted(&self) -> bool {
        self.allowed.is_none()
    }

    fn check_scope(&self, display_name: &str) -> Result<(), String> {
        match &self.allowed {
            None => Ok(()), // unrestricted — all agents visible
            Some(set) if set.is_empty() => {
                Err("Agent state cap denied — operator allowlist is empty".into())
            }
            Some(set) if set.contains(display_name) => Ok(()),
            Some(_) => Err(format!(
                "Agent '{display_name}' is outside the allowed set for this capability"
            )),
        }
    }
}

impl AgentStateAdmin for ScopedAgentStateAdmin {
    fn resolve_agent(&self, name: &str) -> Result<DbEntry, String> {
        // Resolve via HostedIndex (name or DB id). The HostedIndex is
        // held inside the wrapper — tools never see it — so the ambient
        // authority of "enumerate all hosted agents" is contained within
        // the trusted wrapper code.
        let entry = if let Some(e) = self.index.find_by_name(name) {
            e
        } else if let Ok(id) = eidetica::entry::ID::parse(name)
            && let Some(e) = self.index.find_by_id(&id)
        {
            e
        } else {
            return Err(format!("No hosted agent matches '{name}'"));
        };

        // Scope check: the agent's display name must be in the
        // operator-configured allowlist.
        self.check_scope(&entry.display_name)?;
        Ok(entry)
    }

    fn open_agent_db<'a>(
        &'a self,
        entry: &'a DbEntry,
    ) -> CapFuture<'a, AgentDb> {
        Box::pin(async move {
            // Defense in depth — the entry should have come through
            // `resolve_agent`, but verify the scope anyway.
            self.check_scope(&entry.display_name)
                .map_err(anyhow::Error::msg)?;

            let agent_db = self
                .registry
                .open_agent_db(&entry.db_id, Some(&entry.pubkey))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to open agent DB: {e}"))?
                .ok_or_else(|| {
                    anyhow::anyhow!("No key for agent '{}' DB", entry.display_name)
                })?;
            Ok(agent_db)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;

    async fn fixture(agent_names: Vec<String>) -> (Arc<SessionRegistry>, HostedIndex) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
        let agents_reg = Arc::new(AgentRegistry::with_default_agent());
        let registry = Arc::new(
            SessionRegistry::new(instance, user, agents_reg)
                .await
                .unwrap(),
        );
        let index = HostedIndex::empty("agent");

        {
            let mut user = registry.user_for_tests().await;
            for name in agent_names {
                let (agent_db, pubkey) = create_agent_db(
                    &mut user,
                    &name,
                    &AgentDbConfig::default(),
                    &AgentMeta {
                        display_name: Some(name.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                index.register(DbEntry {
                    db_id: agent_db.id(),
                    display_name: name,
                    pubkey,
                });
            }
        } // drop user guard before moving registry

        (registry, index)
    }

    #[tokio::test]
    async fn scoped_resolve_allows_known_agent() {
        let (registry, index) = fixture(vec!["alpha".into()]).await;
        let scope = ScopedAgentStateAdmin::new(
            registry,
            index,
            Some(vec!["alpha".into()]),
        );
        let entry = scope.resolve_agent("alpha").unwrap();
        assert_eq!(entry.display_name, "alpha");
    }

    #[tokio::test]
    async fn scoped_resolve_rejects_unknown_agent() {
        let (registry, index) = fixture(vec!["alpha".into(), "beta".into()]).await;
        let scope = ScopedAgentStateAdmin::new(
            registry,
            index,
            Some(vec!["alpha".into()]),
        );
        let err = scope.resolve_agent("beta").unwrap_err();
        assert!(
            format!("{err:#}").contains("outside the allowed set"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn scoped_resolve_resolves_by_id_and_checks_scope() {
        let (registry, index) = fixture(vec!["gamma".into()]).await;
        let gamma_entry = index.find_by_name("gamma").unwrap();
        let gamma_id = gamma_entry.db_id.to_string();

        let scope = ScopedAgentStateAdmin::new(
            registry,
            index,
            Some(vec!["gamma".into()]),
        );
        let entry = scope.resolve_agent(&gamma_id).unwrap();
        assert_eq!(entry.display_name, "gamma");
    }

    #[tokio::test]
    async fn scoped_resolve_by_id_rejects_scoped_out_agent() {
        let (registry, index) = fixture(vec!["alpha".into()]).await;
        let alpha_entry = index.find_by_name("alpha").unwrap();
        let alpha_id = alpha_entry.db_id.to_string();

        let scope = ScopedAgentStateAdmin::new(
            registry,
            index,
            Some(vec![]), // empty = deny all
        );
        let err = scope.resolve_agent(&alpha_id).unwrap_err();
        assert!(
            format!("{err:#}").contains("allowlist is empty"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn scoped_open_db_rejects_scoped_out_entry() {
        let (registry, index) = fixture(vec!["alpha".into(), "beta".into()]).await;
        let beta_entry = index.find_by_name("beta").unwrap();

        let scope = ScopedAgentStateAdmin::new(
            registry,
            index,
            Some(vec!["alpha".into()]),
        );
        let err = scope
            .open_agent_db(&beta_entry)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("outside the allowed set"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn scoped_open_db_succeeds_for_allowed_agent() {
        let (registry, index) = fixture(vec!["alpha".into()]).await;
        let alpha_entry = index.find_by_name("alpha").unwrap();

        let scope = ScopedAgentStateAdmin::new(
            registry,
            index,
            Some(vec!["alpha".into()]),
        );
        let db = scope
            .open_agent_db(&alpha_entry)
            .await
            .unwrap();
        // Verify the DB opened successfully — the id chain matches.
        assert_eq!(db.id(), alpha_entry.db_id);
    }

    #[tokio::test]
    async fn none_allowlist_is_unrestricted() {
        let (registry, index) = fixture(vec!["alpha".into()]).await;
        let scope = ScopedAgentStateAdmin::new(registry, index, None);
        assert!(scope.is_unrestricted());
        scope.resolve_agent("alpha").unwrap();
    }

    #[tokio::test]
    async fn empty_allowlist_denies_all() {
        let (registry, index) = fixture(vec!["alpha".into()]).await;
        let scope = ScopedAgentStateAdmin::new(
            registry,
            index,
            Some(vec![]),
        );
        let err = scope.resolve_agent("alpha").unwrap_err();
        assert!(format!("{err:#}").contains("allowlist is empty"));
    }
}
