//! Host-only native memory import. Explicit account authority, not a model cap.
//!
//! Like `ScopedAgentStateAdmin`, this is a guardrail for trusted same-account
//! code, not a sandbox. A bank Read reference or extension manifest grants none
//! of these operations. No server, session, tool, embedder or loader is needed.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use eidetica::auth::{
    crypto::PublicKey,
    types::{AuthKey, Permission},
};
use eidetica::store::{DocStore, Table};
use eidetica::{AccountSession, Database};
use serde::{Deserialize, Serialize};

use crate::agent_db::{AgentDb, BankPermission, MemoryBankRef, MemoryEntry};
use crate::memory_bank_db::{MEMORY_STORE, MemoryBankDb, MemoryBankMeta, create_memory_bank};

/// All identity and persisted-layout choices belong to the native caller.
/// `agent_name` is the exact native name without the `agent:` prefix.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryImportScope {
    pub agent_name: String,
    pub expected_agent_display_name: String,
    pub bank_name: String,
    /// Persisted in `state_store.owner` and required as each row's first tag.
    pub owner: String,
    /// Existing DocStore layout: `owner`, `attached`, `hash`, `source` strings.
    pub state_store: String,
    pub row_prefix: String,
}

/// A stable caller-owned row. Timestamps are generated only for changed rows.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryImportRow {
    pub id: String,
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
}

/// Complete intended namespace, not a patch. Empty rows explicitly remove it.
/// Source eligibility, hashing, chunking and provenance are caller policy.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryImportSnapshot {
    pub source: String,
    pub hash: String,
    pub rows: Vec<MemoryImportRow>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryImportChanges {
    pub inserted: usize,
    pub updated: usize,
    pub removed: usize,
    pub state_changed: bool,
}

/// Opaque one-bank native administrative endpoint. Never exposes Chaz DBs,
/// keys, registry or ToolContext. Construction may bootstrap the bank, but never
/// the agent. Each replacement re-resolves identity and rechecks current grants.
/// Serialize this importer with other native setup/ACL writers, as with ordinary
/// Chaz administrative operations; cross-database bootstrap is not atomic.
pub struct NativeMemoryImport<'a> {
    user: &'a AccountSession,
    scope: MemoryImportScope,
    bank_id: String,
    agent_id: String,
    admin: PublicKey,
    reader: PublicKey,
}

impl<'a> NativeMemoryImport<'a> {
    pub async fn bind(user: &'a mut AccountSession, scope: MemoryImportScope) -> Result<Self> {
        scope.validate()?;
        let (agent, reader) = resolve_agent(user, &scope).await?;
        let name = format!("memory:{}", scope.bank_name);
        let mut banks = named_databases(user, &name).await?;
        ensure!(banks.len() <= 1, "ambiguous bank name");
        let (bank, admin) = if let Some((db, key)) = banks.pop() {
            (MemoryBankDb::from_database(db), key)
        } else {
            ensure!(
                user.find_keys_by_display_name(&name).is_empty(),
                "orphaned importer key"
            );
            // A dangling reference must not be silently redirected to a new DB.
            ensure!(
                references(&agent, &scope).await?.is_empty(),
                "orphaned bank reference"
            );
            let (bank, key) = create_memory_bank(
                user,
                &scope.bank_name,
                &MemoryBankMeta {
                    display_name: Some(scope.bank_name.clone()),
                    description: None,
                },
            )
            .await?;
            let tx = bank.database().new_transaction().await?;
            tx.get_store::<DocStore>(&scope.state_store)
                .await?
                .set_string("owner", &scope.owner)
                .await?;
            tx.commit().await?;
            (bank, key)
        };
        let endpoint = Self {
            user,
            scope,
            bank_id: bank.id().to_string(),
            agent_id: agent.id().to_string(),
            admin,
            reader,
        };
        let needs_grant = endpoint.check_bank(&bank).await?;
        let attached = endpoint.attached(&bank).await?;
        endpoint
            .check_reference(&agent, &bank, attached, needs_grant)
            .await?;
        if needs_grant {
            let tx = bank.database().new_transaction().await?;
            tx.get_settings()?
                .set_auth_key(
                    &endpoint.reader,
                    AuthKey::active(Some("memory-import-reader"), Permission::Read),
                )
                .await?;
            tx.commit().await?;
        }
        if !attached {
            // Bank authorization precedes the persistent ref; the completion
            // marker comes last. A partial initial setup is safely retryable.
            agent
                .attach_memory_bank(MemoryBankRef {
                    name: endpoint.scope.bank_name.clone(),
                    db_id: endpoint.bank_id.clone(),
                    permission: BankPermission::Read,
                })
                .await?;
            let tx = bank.database().new_transaction().await?;
            tx.get_store::<DocStore>(&endpoint.scope.state_store)
                .await?
                .set_string("attached", "true")
                .await?;
            tx.commit().await?;
        }
        endpoint.current_bank().await?;
        Ok(endpoint)
    }

    /// Stable identity data, not a database handle or key.
    pub fn bank_id(&self) -> &str {
        &self.bank_id
    }

    /// Atomically replace only the bound row namespace and source/hash state.
    /// Semantic equality ignores timestamps, so unchanged snapshots do not commit.
    /// Invalid input or ownership collisions commit neither rows nor state.
    pub async fn replace_snapshot(
        &mut self,
        snapshot: &MemoryImportSnapshot,
    ) -> Result<MemoryImportChanges> {
        let mut desired = BTreeMap::new();
        for row in &snapshot.rows {
            ensure!(
                row.id.starts_with(&self.scope.row_prefix),
                "row outside importer namespace"
            );
            ensure!(
                row.tags.first() == Some(&self.scope.owner),
                "import row ownership conflict"
            );
            ensure!(
                desired.insert(row.id.as_str(), row).is_none(),
                "duplicate import row ID"
            );
        }
        let bank = self.current_bank().await?;
        let tx = bank.database().new_transaction().await?;
        let table = tx.get_store::<Table<MemoryEntry>>(MEMORY_STORE).await?;
        let old: BTreeMap<_, _> = table
            .search(|_: &MemoryEntry| true)
            .await?
            .into_iter()
            .collect();
        // Validate the entire existing namespace before staging any mutations.
        for (id, row) in &old {
            if id.starts_with(&self.scope.row_prefix) {
                ensure!(
                    row.tags.first() == Some(&self.scope.owner),
                    "import row ownership conflict"
                );
            }
        }
        let mut changes = MemoryImportChanges::default();
        for id in old
            .keys()
            .filter(|id| id.starts_with(&self.scope.row_prefix))
        {
            if !desired.contains_key(id.as_str()) {
                table.delete(id).await?;
                changes.removed += 1;
            }
        }
        for (id, row) in desired {
            if let Some(previous) = old.get(id) {
                if previous.key == row.key
                    && previous.value == row.value
                    && previous.tags == row.tags
                {
                    continue;
                }
                changes.updated += 1;
            } else {
                changes.inserted += 1;
            }
            table
                .set(
                    id,
                    MemoryEntry {
                        key: row.key.clone(),
                        value: row.value.clone(),
                        tags: row.tags.clone(),
                        timestamp: chrono::Utc::now(),
                    },
                )
                .await?;
        }
        let state = tx.get_store::<DocStore>(&self.scope.state_store).await?;
        let saved = state.get_all().await?;
        changes.state_changed = saved
            .get("hash")
            .and_then(|v| String::try_from(v).ok())
            .as_deref()
            != Some(&snapshot.hash)
            || saved
                .get("source")
                .and_then(|v| String::try_from(v).ok())
                .as_deref()
                != Some(&snapshot.source);
        if changes.state_changed {
            state.set_string("hash", &snapshot.hash).await?;
            state.set_string("source", &snapshot.source).await?;
        }
        if changes.state_changed || changes.inserted + changes.updated + changes.removed > 0 {
            tx.commit().await?;
        }
        Ok(changes)
    }

    async fn current_bank(&self) -> Result<MemoryBankDb> {
        let (agent, reader) = resolve_agent(self.user, &self.scope).await?;
        ensure!(
            agent.id().to_string() == self.agent_id && reader == self.reader,
            "bound agent identity changed"
        );
        let mut banks =
            named_databases(self.user, &format!("memory:{}", self.scope.bank_name)).await?;
        ensure!(banks.len() == 1, "bound bank is missing or ambiguous");
        let (db, admin) = banks.pop().unwrap();
        let bank = MemoryBankDb::from_database(db);
        ensure!(
            bank.id().to_string() == self.bank_id && admin == self.admin,
            "bound bank identity changed"
        );
        let needs_grant = self.check_bank(&bank).await?;
        ensure!(
            self.attached(&bank).await?,
            "bank attachment marker missing"
        );
        self.check_reference(&agent, &bank, true, needs_grant)
            .await?;
        Ok(bank)
    }

    async fn attached(&self, bank: &MemoryBankDb) -> Result<bool> {
        let tx = bank.database().new_transaction().await?;
        Ok(tx
            .get_store::<DocStore>(&self.scope.state_store)
            .await?
            .get_all()
            .await?
            .get("attached")
            .is_some())
    }

    /// Returns whether an absent (never revoked/inactive) reader grant needs bootstrap.
    async fn check_bank(&self, bank: &MemoryBankDb) -> Result<bool> {
        ensure!(
            !bank.database().is_shared().await?,
            "importer bank sharing enabled"
        );
        let tx = bank.database().new_transaction().await?;
        ensure!(
            tx.get_store::<DocStore>(&self.scope.state_store)
                .await?
                .get_string("owner")
                .await?
                == self.scope.owner,
            "bank is not owned by this importer"
        );
        let auth = tx.get_settings()?.auth_snapshot().await?;
        ensure!(
            auth.as_doc().get("global").is_none() && auth.as_doc().get("delegations").is_none(),
            "global/delegated bank access is not approved"
        );
        let mut owner_admin = false;
        let mut reader_found = false;
        for (key, grant) in auth.get_all_keys()? {
            if key == self.reader.to_string() {
                ensure!(
                    grant.is_active() && *grant.permissions() == Permission::Read,
                    "existing agent grant is not active Read"
                );
                reader_found = true;
            } else if key == self.admin.to_string() {
                ensure!(
                    grant.is_active() && grant.permissions().can_admin(),
                    "importer owner is not active admin"
                );
                owner_admin = true;
            } else {
                ensure!(!grant.is_active(), "unexpected bank reader/writer");
            }
        }
        ensure!(owner_admin, "importer owner is not active admin");
        Ok(!reader_found)
    }

    async fn check_reference(
        &self,
        agent: &AgentDb,
        bank: &MemoryBankDb,
        attached: bool,
        needs_grant: bool,
    ) -> Result<()> {
        let refs = references(agent, &self.scope).await?;
        ensure!(refs.len() <= 1, "ambiguous bank reference");
        if attached {
            ensure!(
                !needs_grant && refs.len() == 1,
                "bank was detached/revoked; refusing automatic reattachment"
            );
        }
        for reference in refs {
            ensure!(
                reference.db_id == bank.id().to_string()
                    && reference.permission == BankPermission::Read,
                "conflicting bank reference"
            );
        }
        Ok(())
    }
}

impl MemoryImportScope {
    fn validate(&self) -> Result<()> {
        ensure!(
            [
                &self.agent_name,
                &self.expected_agent_display_name,
                &self.bank_name,
                &self.owner,
                &self.state_store,
                &self.row_prefix
            ]
            .iter()
            .all(|s| !s.is_empty()),
            "empty importer scope"
        );
        ensure!(
            ![MEMORY_STORE, crate::memory_bank_db::META_STORE, "_settings"]
                .contains(&self.state_store.as_str()),
            "import state overlaps a reserved store"
        );
        Ok(())
    }
}

async fn references(agent: &AgentDb, scope: &MemoryImportScope) -> Result<Vec<MemoryBankRef>> {
    Ok(agent
        .list_memory_banks()
        .await?
        .into_iter()
        .filter(|r| r.name == scope.bank_name)
        .collect())
}

// Use catalog-tracked keys and propagate discovery/open/name-read errors. SDK
// find_database/find_key can hide failures or choose another account-held key.
async fn named_databases(user: &AccountSession, name: &str) -> Result<Vec<(Database, PublicKey)>> {
    let mut found = Vec::new();
    for tracked in user.databases().await? {
        let db = user
            .open_database_with_key(&tracked.database_id, &tracked.key_id)
            .await?;
        if db.get_name().await? == name {
            found.push((db, tracked.key_id));
        }
    }
    Ok(found)
}

async fn resolve_agent(
    user: &AccountSession,
    scope: &MemoryImportScope,
) -> Result<(AgentDb, PublicKey)> {
    let mut agents = named_databases(user, &format!("agent:{}", scope.agent_name)).await?;
    ensure!(
        agents.len() == 1,
        "approved existing agent is missing or ambiguous"
    );
    let (db, reader) = agents.pop().unwrap();
    let agent = AgentDb::from_database(db);
    ensure!(
        agent.read_meta().await?.display_name.as_deref()
            == Some(&scope.expected_agent_display_name),
        "approved agent identity mismatch"
    );
    Ok((agent, reader))
}

#[cfg(test)]
mod tests;
