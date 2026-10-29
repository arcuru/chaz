//! Provider catalog metadata only: no Provider DB lifecycle, native attachment,
//! operating-key proof, or runtime integration.

use super::{AgentDb, AgentDbConfig, BLOB_KEY, CONFIG_STORE, PROVIDERS_STORE};
use anyhow::Context;
use eidetica::entry::ID;
use eidetica::store::{DocStore, Table};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// A known Provider, keyed by its stable Eidetica DB root, not its label.
/// Catalog membership and selection grant no session or resource access.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderRef {
    pub db_id: String,
    pub display_name: String,
    /// Routing hint only, never a principal or proof of an operating key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_pubkey: Option<String>,
    /// Routing hints as `(transport, address)` pairs, like `LoginRef`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sync_addresses: Vec<(String, String)>,
}

fn validate_provider_id(db_id: &str) -> anyhow::Result<()> {
    let id = ID::parse(db_id).context("invalid Provider DB ID")?;
    anyhow::ensure!(!id.is_empty(), "invalid Provider DB ID: empty root");
    anyhow::ensure!(
        id.to_string() == db_id,
        "noncanonical Provider DB ID: {db_id}"
    );
    Ok(())
}

fn provider_catalog(catalog: &[ProviderRef]) -> anyhow::Result<BTreeMap<&str, &ProviderRef>> {
    let mut by_id = BTreeMap::new();
    for provider in catalog {
        validate_provider_id(&provider.db_id)?;
        if let Some(previous) = by_id.insert(provider.db_id.as_str(), provider) {
            anyhow::ensure!(
                previous == provider,
                "conflicting Provider catalog entries for {}",
                provider.db_id
            );
        }
    }
    Ok(by_id)
}

/// Compute a metadata-only selected set, sorted and deduplicated by DB root.
/// `None` uses `defaults`; `Some(&[])` selects none. An override replaces, not
/// extends, defaults. No label lookup or legacy default-agent fallback occurs.
///
/// Invalid catalog IDs, conflicting same-ID rows, or invalid/unknown selected
/// IDs fail the whole computation. Identical duplicate catalog rows are harmless.
/// This validates references, not Provider authority or present availability.
pub fn select_providers(
    catalog: &[ProviderRef],
    defaults: &[String],
    override_ids: Option<&[String]>,
) -> anyhow::Result<Vec<ProviderRef>> {
    let by_id = provider_catalog(catalog)?;
    override_ids
        .unwrap_or(defaults)
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|id| {
            validate_provider_id(id)?;
            by_id
                .get(id.as_str())
                .map(|provider| (*provider).clone())
                .with_context(|| format!("unknown Provider DB ID: {id}"))
        })
        .collect()
}

impl AgentDb {
    /// Inspect the catalog in stable-ID order. Refuses conflicting same-ID rows
    /// rather than picking an arbitrary winner after concurrent metadata writes.
    pub async fn list_providers(&self) -> anyhow::Result<Vec<ProviderRef>> {
        let txn = self.database().new_transaction().await?;
        let catalog =
            read_catalog(&txn.get_store::<Table<ProviderRef>>(PROVIDERS_STORE).await?).await?;
        Ok(provider_catalog(&catalog)?.into_values().cloned().collect())
    }

    /// Find by exact DB root, never by display name or routing hint.
    pub async fn find_provider(&self, db_id: &str) -> anyhow::Result<Option<ProviderRef>> {
        validate_provider_id(db_id)?;
        Ok(self
            .list_providers()
            .await?
            .into_iter()
            .find(|provider| provider.db_id == db_id))
    }

    /// Add or replace metadata for this exact root. Native AgentDB Write is
    /// required. All same-ID rows are replaced; another root with the same label
    /// is a different Provider and is not retargeted or removed.
    pub async fn upsert_provider(&self, provider: ProviderRef) -> anyhow::Result<()> {
        validate_provider_id(&provider.db_id)?;
        let txn = self.database().new_transaction().await?;
        let store = txn.get_store::<Table<ProviderRef>>(PROVIDERS_STORE).await?;
        for (row_id, _) in store
            .search(|row: &ProviderRef| row.db_id == provider.db_id)
            .await?
        {
            store.delete(&row_id).await?;
        }
        store.insert(provider).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Persist validated default IDs, deduplicated in stable-ID order. Native
    /// AgentDB Write is required; unrelated config (including unknown fields)
    /// is preserved. Catalog validation and the config edit share a transaction.
    pub async fn set_default_providers(&self, ids: &[String]) -> anyhow::Result<()> {
        let txn = self.database().new_transaction().await?;
        let catalog =
            read_catalog(&txn.get_store::<Table<ProviderRef>>(PROVIDERS_STORE).await?).await?;
        let selected = select_providers(&catalog, &[], Some(ids))?;
        let store = txn.get_store::<DocStore>(CONFIG_STORE).await?;
        let (mut raw, _) = read_provider_config(&store).await?;
        let ids: Vec<_> = selected
            .into_iter()
            .map(|provider| provider.db_id)
            .collect();
        raw.insert("default_providers".into(), serde_json::to_value(ids)?);
        store
            .set_string(BLOB_KEY, serde_json::to_string(&raw)?)
            .await?;
        txn.commit().await?;
        Ok(())
    }

    /// Remove all rows for a root and prune it from future defaults in the same
    /// native AgentDB transaction. Returns whether a catalog row was removed;
    /// also prunes a dangling default when the row is already absent. Retry is
    /// harmless. Existing sessions/attachments/access are never changed.
    ///
    /// This is a local transaction boundary, not global CRDT locking. Later
    /// sync can still introduce inconsistencies; selection must revalidate.
    pub async fn remove_provider(&self, db_id: &str) -> anyhow::Result<bool> {
        validate_provider_id(db_id)?;
        let txn = self.database().new_transaction().await?;
        let catalog = txn.get_store::<Table<ProviderRef>>(PROVIDERS_STORE).await?;
        let existing = catalog
            .search(|row: &ProviderRef| row.db_id == db_id)
            .await?;
        let removed = !existing.is_empty();
        for (row_id, _) in existing {
            catalog.delete(&row_id).await?;
        }
        let store = txn.get_store::<DocStore>(CONFIG_STORE).await?;
        let (mut raw, mut config) = read_provider_config(&store).await?;
        let before = config.default_providers.len();
        config.default_providers.retain(|id| id != db_id);
        if config.default_providers.len() != before {
            raw.insert(
                "default_providers".into(),
                serde_json::to_value(config.default_providers)?,
            );
            store
                .set_string(BLOB_KEY, serde_json::to_string(&raw)?)
                .await?;
        }
        txn.commit().await?;
        Ok(removed)
    }
}

async fn read_catalog(store: &Table<ProviderRef>) -> anyhow::Result<Vec<ProviderRef>> {
    Ok(store
        .search(|_: &ProviderRef| true)
        .await?
        .into_iter()
        .map(|(_, row)| row)
        .collect())
}

// Edit only the Provider-default field of the existing config blob. Validate the
// known schema first, but retain unknown fields for newer/other config writers.
// A failed read must never replace unreadable state with default configuration.
async fn read_provider_config(
    store: &DocStore,
) -> anyhow::Result<(Map<String, Value>, AgentDbConfig)> {
    let raw: Map<String, Value> = match store.get_string(BLOB_KEY).await {
        Ok(json) => serde_json::from_str(&json)?,
        Err(error) if error.is_not_found() => Map::new(),
        Err(error) => return Err(error.into()),
    };
    let config = serde_json::from_value(Value::Object(raw.clone()))?;
    Ok((raw, config))
}
