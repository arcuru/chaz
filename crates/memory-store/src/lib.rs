//! The persisted memory table shared by Chaz agents, banks and external writers.
//! Discovery, Auth provisioning, recall and source selection live elsewhere.

use std::collections::HashSet;

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use eidetica::{
    Database, Transaction,
    store::{StoreError, Table},
};
use serde::{Deserialize, Serialize};

pub const MEMORY_STORE: &str = "memory";

/// A single fact in the native memory table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub timestamp: DateTime<Utc>,
    /// Free-form labels. Older rows may lack this field.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Open the standard table inside a caller-owned transaction. Chaz can still
/// commit a memory together with its embedding; neither is required to read it.
pub async fn memory_table(tx: &Transaction) -> eidetica::Result<Table<MemoryEntry>> {
    tx.get_store(MEMORY_STORE).await
}

/// Replacement content for a caller-assigned stable row ID. The ID is the
/// table key, not the `key` displayed in recall results.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryUpsert {
    pub id: String,
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MemoryChanges {
    pub inserted: usize,
    pub updated: usize,
    pub removed: usize,
}

/// Apply only the supplied IDs in one transaction on an already-open database.
/// Repeating the same changes preserves timestamps and makes no new commit.
/// The caller selects its own IDs, provenance and stale rows, and serializes
/// refreshes for the same IDs, including any read used to decide the deletes.
/// Bank Write is bank-wide; this helper is not an ACL or a conflict detector.
pub async fn apply_batch(
    database: &Database,
    upserts: &[MemoryUpsert],
    deletes: &[String],
) -> Result<MemoryChanges> {
    let mut ids = HashSet::new();
    for id in upserts.iter().map(|row| &row.id).chain(deletes) {
        ensure!(!id.is_empty(), "empty memory row ID");
        ensure!(ids.insert(id), "duplicate memory row ID: {id}");
    }

    let tx = database.new_transaction().await?;
    let table = memory_table(&tx).await?;
    let mut changes = MemoryChanges::default();
    for row in upserts {
        match read_row(&table, &row.id).await? {
            Some(old) if old.key == row.key && old.value == row.value && old.tags == row.tags => {
                continue;
            }
            Some(_) => changes.updated += 1,
            None => changes.inserted += 1,
        }
        table
            .set(
                &row.id,
                MemoryEntry {
                    key: row.key.clone(),
                    value: row.value.clone(),
                    timestamp: Utc::now(),
                    tags: row.tags.clone(),
                },
            )
            .await?;
    }
    for id in deletes {
        // Table::delete treats failed row reads as absence. Propagate read
        // failures instead of silently committing only the upserts.
        if read_row(&table, id).await?.is_some() {
            ensure!(table.delete(id).await?, "could not delete memory row: {id}");
            changes.removed += 1;
        }
    }
    if changes != MemoryChanges::default() {
        tx.commit().await?;
    }
    Ok(changes)
}

async fn read_row(table: &Table<MemoryEntry>, id: &str) -> eidetica::Result<Option<MemoryEntry>> {
    match table.get(id).await {
        Ok(row) => Ok(Some(row)),
        Err(eidetica::Error::Store(error)) if matches!(*error, StoreError::KeyNotFound { .. }) => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidetica::{
        Instance, NewAccount, auth::crypto::generate_keypair, backend::database::InMemory,
        crdt::Doc,
    };

    #[tokio::test]
    async fn conflicting_or_unreadable_batch_never_commits_an_upsert() {
        let (instance, _) =
            Instance::create_backend(Box::new(InMemory::new()), NewAccount::passwordless("test"))
                .await
                .unwrap();
        let db = Database::create(&instance, generate_keypair().0, Doc::new())
            .await
            .unwrap();
        let tx = db.new_transaction().await.unwrap();
        tx.get_store::<Table<serde_json::Value>>(MEMORY_STORE)
            .await
            .unwrap()
            .set("unreadable", serde_json::json!({"key": "missing fields"}))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let row = [MemoryUpsert {
            id: "new".into(),
            key: "fact".into(),
            value: "text".into(),
            tags: vec![],
        }];
        let before = db.snapshot().await.unwrap();
        assert!(
            apply_batch(&db, &row, &["new".into()])
                .await
                .unwrap_err()
                .to_string()
                .contains("duplicate memory row ID")
        );
        assert_eq!(db.snapshot().await.unwrap(), before);
        assert!(matches!(
            apply_batch(&db, &row, &["unreadable".into()]).await.unwrap_err().downcast::<eidetica::Error>(),
            Ok(eidetica::Error::Store(err)) if matches!(*err, StoreError::DeserializationFailed { .. })
        ));
        assert_eq!(db.snapshot().await.unwrap(), before);
        // Positive control: without the failing second operation the row commits.
        assert_eq!(apply_batch(&db, &row, &[]).await.unwrap().inserted, 1);
        assert_ne!(db.snapshot().await.unwrap(), before);
    }
}
