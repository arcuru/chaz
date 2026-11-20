//! Read-only virtual anchors into the original legacy stores. No record is
//! rewritten or made current by this adapter. Mixed-view selection reuses the
//! legacy renderer and complete native-turn groups without copying their owners.
use eidetica::{Database, Snapshot, store::Table};
use serde::{Deserialize, Serialize};

use crate::extension::durable_context::CommittedContribution;
use crate::session::{ContextGroup, EntryType, HistoryOrigin, Session, SessionCommandResult};

use super::{ConversationError, Result, atom, contract::LegacyBoundary, record::RepresentationId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyReference {
    pub database: String,
    pub frontier: Snapshot,
    pub store: String,
    pub row: String,
    pub variant: String,
    pub index: u32,
    /// Digest of the version-one typed row encoding, not of guessed original
    /// transport bytes. The immutable address also pins the selected main DAG.
    pub record_digest: String,
    pub format: u32,
}
impl LegacyReference {
    pub fn representation_id(&self) -> Result<RepresentationId> {
        Ok(RepresentationId {
            database: self.database.clone(),
            operation: format!("legacy:{}", atom::key(&atom::encode(self)?)),
            index: self.index,
        })
    }
}

#[derive(Clone, Debug)]
pub struct LegacyAnchor {
    pub ordinal: u64,
    pub reference: LegacyReference,
}
impl LegacyAnchor {
    /// Stable epoch-zero base key. New appends use epoch one; a replacement
    /// must inherit this complete anchor rather than renumber its surviving rows.
    pub fn base_key(&self) -> Result<(u8, u64, String, u32)> {
        Ok((
            0,
            self.ordinal,
            atom::key(&atom::encode(&self.reference)?),
            self.reference.index,
        ))
    }
}

#[derive(Clone, Debug)]
pub(super) struct LegacyEntry {
    pub anchor: LegacyAnchor,
    pub group: ContextGroup,
}

#[derive(Clone, Debug, Default)]
pub(super) struct LegacyView {
    pub entries: Vec<LegacyEntry>,
    pub contributions: Vec<CommittedContribution>,
    pub receipts: Vec<String>,
}

fn reference<T: Serialize>(
    db: &Database,
    frontier: &Snapshot,
    store: &str,
    row: &str,
    variant: &str,
    value: &T,
) -> Result<LegacyReference> {
    Ok(LegacyReference {
        database: db.root_id().to_string(),
        frontier: frontier.clone(),
        store: store.into(),
        row: row.into(),
        variant: variant.into(),
        index: 0,
        record_digest: atom::key(&atom::encode(value)?),
        format: 1,
    })
}

/// Pin the existing validated context-view order at the exact legacy boundary.
/// This is a historical reader, never activation permission. Pending jobs/turns
/// must be reconciled by the eventual activation procedure, not guessed here.
pub(super) async fn at(db: &Database, boundary: &LegacyBoundary) -> Result<LegacyView> {
    let history = Session::history_at(db, &boundary.frontier).await?;
    history.require_legacy()?;
    let mut receipts = Vec::new();
    let rows = history.entries().await?;
    for (_, entry) in &rows {
        if matches!(entry.entry_type, EntryType::Unknown { .. })
            || (entry.entry_type == EntryType::BridgeEvent && entry.bridge_role().is_none())
        {
            return Err(ConversationError::Unsupported(
                "unknown legacy entry semantics".into(),
            ));
        }
        if entry.bridge_role() == Some(crate::session::BridgeEventRole::Receipt)
            && let Some(reply) = entry
                .routing
                .as_ref()
                .and_then(|routing| routing.reply_to.clone())
        {
            receipts.push(reply);
        }
    }
    let (groups, contributions, _) = history.legacy_context(rows, None, &|_| true).await?;
    let txn = db.new_transaction_at(&boundary.frontier).await?;
    let cutoff = groups
        .iter()
        .rposition(|row| row.entry.entry_type == EntryType::Summary)
        .unwrap_or(0);
    let mut entries = Vec::new();
    for (ordinal, group) in groups.into_iter().enumerate().skip(cutoff) {
        if !crate::context::is_context_entry(&group.entry) {
            continue;
        }
        if !group.transcript.is_empty()
            && crate::context::replay_turn(
                group.transcript.iter().map(|row| &row.record),
                usize::MAX,
            )
            .is_none()
        {
            return Err(ConversationError::ExchangeConflict(
                "invalid legacy native exchange at boundary".into(),
            ));
        }
        let source = match &group.origin {
            Some(HistoryOrigin::Entry(id)) => reference(
                db,
                &boundary.frontier,
                "entries",
                id.as_str(),
                "entry",
                &group.entry,
            )?,
            Some(HistoryOrigin::CompactCommand(id)) => {
                let result = txn
                    .get_store::<Table<SessionCommandResult>>(
                        super::super::SESSION_COMMAND_RESULTS_STORE,
                    )
                    .await?
                    .get(id)
                    .await?;
                reference(
                    db,
                    &boundary.frontier,
                    super::super::SESSION_COMMAND_RESULTS_STORE,
                    id,
                    "compact-summary",
                    &result,
                )?
            }
            None => return Err(ConversationError::Missing("legacy entry origin".into())),
        };
        // `group.transcript` retains the native row keys, member indices and
        // the exact observed records. No second read or value-based lookup.
        if group.transcript.iter().any(|row| row.id.is_none()) {
            return Err(ConversationError::Missing(
                "native transcript row key".into(),
            ));
        }
        entries.push(LegacyEntry {
            anchor: LegacyAnchor {
                ordinal: ordinal as u64,
                reference: source,
            },
            group,
        });
    }
    Ok(LegacyView {
        entries,
        contributions,
        receipts,
    })
}
