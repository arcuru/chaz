//! Read-only views of legacy session stores at one accepted main frontier.
use super::{
    EntryType, META_STORE, SESSION_COMMAND_RESULTS_STORE, SESSION_COMMANDS_STORE, Session,
    SessionCommand, SessionCommandOutcome, SessionCommandRequest, SessionCommandResult,
    SessionEntry, TURN_ATTEMPTS_STORE, TURN_TRANSCRIPT_STORE, TurnRequestId, TurnRequestState,
    TurnTranscriptRecord, index_attempt_states, is_turn_request, read_turn_baseline_from,
};
use crate::extension::durable_context::{self, CommittedContribution};
use eidetica::backend::Reachability;
use eidetica::store::{DocStore, Table};
use eidetica::{Database, Snapshot, Transaction};
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};
use tracing::warn;

/// A Session history read using the caller's database handle (and therefore
/// its native read scope). Never hands out a transaction or installs a writer.
pub struct HistoryRead<'a> {
    database: &'a Database,
    snapshot: Snapshot,
    txn: Transaction,
}

/// Origin of a context row. A virtual summary refers to the compact command
/// result, not an invented entry row or an equal-looking historical message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryOrigin {
    Entry(TurnRequestId),
    CompactCommand(String),
}

/// A native transcript row and its immutable table key. Member coordinates
/// (model sequence, call index, steering input IDs, etc.) remain in the record.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptRow {
    // Only ad hoc, non-persisted inputs to ContextBuilder lack a native key.
    pub id: Option<String>,
    pub record: TurnTranscriptRecord,
}

/// A timeline row and the selected attempt's native records, in sequence order.
#[derive(Clone, Debug)]
pub struct ContextGroup {
    pub origin: Option<HistoryOrigin>,
    pub entry: SessionEntry,
    pub protected: bool,
    pub transcript: Vec<TranscriptRow>,
}

impl ContextGroup {
    pub fn entry_id(&self) -> Option<&TurnRequestId> {
        match &self.origin {
            Some(HistoryOrigin::Entry(id)) => Some(id),
            _ => None,
        }
    }
}

/// Selected conversation history and committed contributions, not a store or
/// an execution grant. The host can attenuate contributions before exposing it.
#[derive(Clone, Debug)]
pub struct SessionContextView {
    pub groups: Vec<ContextGroup>,
    pub contributions: Vec<CommittedContribution>,
    #[cfg(test)]
    pub(crate) read_volume: serde_json::Value,
}

impl SessionContextView {
    pub fn entries(
        &self,
    ) -> impl DoubleEndedIterator<Item = &SessionEntry> + ExactSizeIterator + '_ {
        self.groups.iter().map(|group| &group.entry)
    }

    pub(super) fn into_flat(self) -> (Vec<SessionEntry>, Vec<Vec<TurnTranscriptRecord>>) {
        self.groups
            .into_iter()
            .map(|group| {
                (
                    group.entry,
                    group.transcript.into_iter().map(|row| row.record).collect(),
                )
            })
            .unzip()
    }
}

impl Session {
    /// Open the current accepted history on the caller's database handle.
    pub async fn history(database: &Database) -> anyhow::Result<HistoryRead<'_>> {
        let snapshot = accepted_frontier(database).await?;
        HistoryRead::open(database, snapshot).await
    }

    /// Open exactly this frontier of the authorized database. A missing,
    /// foreign, or unverified frontier is an error, never a moving fallback.
    pub async fn history_at<'a>(
        database: &'a Database,
        snapshot: &Snapshot,
    ) -> anyhow::Result<HistoryRead<'a>> {
        let accepted = accepted_frontier(database).await?;
        if snapshot != &accepted {
            let reachable = if let Some(engine) = database.backend()?.local_engine() {
                engine
                    .check_targets_reachable_from(
                        database.root_id(),
                        accepted.tips(),
                        snapshot.tips(),
                    )
                    .await?
                    == Reachability::Reachable
            } else {
                remote_reaches(database, &accepted, snapshot).await?
            };
            anyhow::ensure!(
                reachable,
                "history frontier is not in the accepted session history"
            );
        }
        HistoryRead::open(database, snapshot.clone()).await
    }
}

async fn accepted_frontier(database: &Database) -> anyhow::Result<Snapshot> {
    if let Some(engine) = database.backend()?.local_engine() {
        // Don't invoke the opportunistic verification in Database::snapshot
        // on a local read, or honor a diagnostic allow_unverified flag.
        Ok(engine.verified_snapshot(database.root_id()).await?)
    } else {
        // The service answers using Verified scope and checks the handle's key.
        Ok(database.snapshot().await?)
    }
}

// The backend reachability primitive is local-only. Walk the accepted prefix
// with the per-database client identity, not the connection's login identity.
async fn remote_reaches(
    database: &Database,
    accepted: &Snapshot,
    selected: &Snapshot,
) -> anyhow::Result<bool> {
    let mut remaining: HashSet<_> = selected.tips().iter().cloned().collect();
    let mut floor = u64::MAX;
    for id in &remaining {
        floor = floor.min(database.get_entry(id).await?.height());
    }
    let mut pending = accepted.tips().to_vec();
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        remaining.remove(&id);
        if remaining.is_empty() {
            return Ok(true);
        }
        if seen.insert(id.clone()) {
            let entry = database.get_entry(id).await?;
            if entry.height() > floor {
                pending.extend(entry.parents()?);
            }
        }
    }
    Ok(remaining.is_empty())
}

impl HistoryRead<'_> {
    async fn open(database: &Database, snapshot: Snapshot) -> anyhow::Result<HistoryRead<'_>> {
        let txn = database.new_transaction_at(&snapshot).await?;
        Ok(HistoryRead {
            database,
            snapshot,
            txn,
        })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Raw display rows in legacy transcript order, before any compaction.
    pub async fn entries(&self) -> anyhow::Result<Vec<(TurnRequestId, SessionEntry)>> {
        entry_rows(&self.txn, "entries").await
    }

    pub async fn context(&self) -> anyhow::Result<SessionContextView> {
        self.context_for(None, &|_| true).await
    }

    pub async fn turn_context(
        &self,
        current: &TurnRequestId,
        is_agent: impl Fn(&str) -> bool + Sync,
    ) -> anyhow::Result<SessionContextView> {
        self.context_for(Some(current), &is_agent).await
    }

    pub(super) async fn context_entries(&self) -> anyhow::Result<Vec<SessionEntry>> {
        Ok(self
            .compacted_groups(&|_, _| false)
            .await?
            .into_iter()
            .map(|group| group.entry)
            .collect())
    }

    /// Preserve the existing compact-command coverage and positional Summary
    /// order. Rows rescued from before a positional Summary follow it; other
    /// rows stay available to custom strategies exactly as before.
    async fn compacted_groups(
        &self,
        pending: &(dyn Fn(&TurnRequestId, &SessionEntry) -> bool + Sync),
    ) -> anyhow::Result<Vec<ContextGroup>> {
        let results = self
            .txn
            .get_store::<Table<SessionCommandResult>>(SESSION_COMMAND_RESULTS_STORE)
            .await?
            .search(|result| matches!(result.outcome, SessionCommandOutcome::Compact { .. }))
            .await?;
        let commands = self
            .txn
            .get_store::<Table<SessionCommandRequest>>(SESSION_COMMANDS_STORE)
            .await?;
        let mut candidates = Vec::new();
        for (id, result) in results {
            let SessionCommandOutcome::Compact { summary } = result.outcome else {
                continue;
            };
            let request = commands.get(&id).await?;
            let SessionCommand::Compact { source_snapshot } = request.command else {
                continue;
            };
            candidates.push((id, result.completed_at, source_snapshot, summary));
        }
        let db = self.database;
        let mut groups = Vec::new();
        let mut covered_ids = HashSet::new();
        if let Some(mut selected) = candidates.pop() {
            for candidate in candidates {
                let selected_to_candidate = db.ids_added(&selected.2, &candidate.2).await?;
                let candidate_to_selected = db.ids_added(&candidate.2, &selected.2).await?;
                let candidate_covers_more =
                    !selected_to_candidate.is_empty() && candidate_to_selected.is_empty();
                let incomparable_or_equal =
                    selected_to_candidate.is_empty() == candidate_to_selected.is_empty();
                if candidate_covers_more
                    || (incomparable_or_equal
                        && (candidate.1, &candidate.0) > (selected.1, &selected.0))
                {
                    selected = candidate;
                }
            }
            covered_ids.extend(
                Session::history_at(db, &selected.2)
                    .await?
                    .entries()
                    .await?
                    .into_iter()
                    .map(|(id, _)| id),
            );
            groups.push(ContextGroup {
                origin: Some(HistoryOrigin::CompactCommand(selected.0)),
                entry: SessionEntry {
                    sender: "system".into(),
                    content: selected.3,
                    timestamp: selected.1,
                    entry_type: EntryType::Summary,
                    metadata: None,
                    routing: None,
                },
                protected: false,
                transcript: Vec::new(),
            });
        }
        groups.extend(self.entries().await?.into_iter().filter_map(|(id, entry)| {
            let protected = pending(&id, &entry);
            (!covered_ids.contains(&id) || protected).then_some(ContextGroup {
                origin: Some(HistoryOrigin::Entry(id)),
                entry,
                protected,
                transcript: Vec::new(),
            })
        }));
        if let Some(boundary) = groups
            .iter()
            .rposition(|group| group.entry.entry_type == EntryType::Summary)
        {
            let mut after = groups.split_off(boundary).into_iter();
            let (rescued, hidden): (Vec<_>, Vec<_>) =
                groups.into_iter().partition(|group| group.protected);
            groups = hidden;
            groups.extend(after.next());
            groups.extend(rescued);
            groups.extend(after);
        }
        Ok(groups)
    }

    async fn context_for(
        &self,
        current: Option<&TurnRequestId>,
        is_agent: &(dyn Fn(&str) -> bool + Sync),
    ) -> anyhow::Result<SessionContextView> {
        let attempts = rows(&self.txn, TURN_ATTEMPTS_STORE).await?;
        #[cfg(test)]
        let attempt_volume = (attempts.len(), serde_json::to_vec(&attempts)?.len());
        let states = index_attempt_states(attempts, &HashSet::new());
        let baseline = match current {
            Some(_) => {
                read_turn_baseline_from(&self.txn.get_store::<DocStore>(META_STORE).await?).await?
            }
            None => HashSet::new(),
        };
        // An interrupted request stays compacted until explicitly retried.
        let mut groups = self
            .compacted_groups(&|id, entry| {
                current.is_some_and(|current| {
                    id == current
                        || (is_turn_request(id, entry, &baseline, is_agent)
                            && !states.contains_key(id))
                })
            })
            .await?;
        groups.retain(|group| group.entry.entry_type != EntryType::TurnContext);
        let contributions = durable_context::read_in(self.database, &self.txn).await?;
        let rows = rows(&self.txn, TURN_TRANSCRIPT_STORE).await?;
        #[cfg(test)]
        let read_volume = serde_json::json!({
            "scope": "context_view Table results only; excludes reopen, metadata, command/state lookups and backend physical I/O",
            "entry_rows": groups.len(), "entry_bytes": serde_json::to_vec(&groups.iter().map(|g| (g.entry_id(), &g.entry)).collect::<Vec<_>>())?.len(),
            "attempt_rows": attempt_volume.0, "attempt_bytes": attempt_volume.1,
            "transcript_rows": rows.len(), "transcript_bytes": serde_json::to_vec(&rows)?.len(),
            "contribution_rows": contributions.len(), "contribution_bytes": serde_json::to_vec(&contributions)?.len(),
        });
        let mut records =
            selected_transcripts(rows, &states, |record| Some(record.request_id.clone()));
        for group in &mut groups {
            if let Some(transcript) = group.entry_id().and_then(|id| records.remove(id)) {
                group.transcript = transcript;
            }
        }
        Ok(SessionContextView {
            groups,
            contributions,
            #[cfg(test)]
            read_volume,
        })
    }

    /// Retained evidence intentionally bypasses compaction; its scoped caller
    /// still checks current participation and source-tool visibility.
    pub(crate) async fn retained_tool_history(&self) -> anyhow::Result<Vec<Vec<TranscriptRow>>> {
        let states =
            index_attempt_states(rows(&self.txn, TURN_ATTEMPTS_STORE).await?, &HashSet::new());
        let entries: HashSet<_> = self
            .entries()
            .await?
            .into_iter()
            .filter(|(_, entry)| {
                matches!(entry.entry_type, EntryType::Message | EntryType::Directive)
            })
            .map(|(id, _)| id)
            .collect();
        // Preserve the audit reader's attempt grouping: inconsistent records
        // claiming one attempt across requests must not replay independently.
        Ok(selected_transcripts(
            rows(&self.txn, TURN_TRANSCRIPT_STORE).await?,
            &states,
            |record| {
                entries
                    .contains(&record.request_id)
                    .then(|| record.attempt_id.clone())
            },
        )
        .into_values()
        .collect())
    }
}

fn selected_transcripts<K: Eq + Hash>(
    rows: Vec<(String, TurnTranscriptRecord)>,
    states: &HashMap<TurnRequestId, TurnRequestState>,
    key: impl Fn(&TurnTranscriptRecord) -> Option<K>,
) -> HashMap<K, Vec<TranscriptRow>> {
    let mut groups: HashMap<_, Vec<_>> = HashMap::new();
    for (id, record) in rows {
        if matches!(states.get(&record.request_id), Some(TurnRequestState::Completed { attempt_id }) if attempt_id == &record.attempt_id)
            && let Some(key) = key(&record)
        {
            groups.entry(key).or_default().push(TranscriptRow {
                id: Some(id),
                record,
            });
        }
    }
    for group in groups.values_mut() {
        group.sort_by(|a, b| {
            a.record
                .sequence
                .cmp(&b.record.sequence)
                .then_with(|| a.id.cmp(&b.id))
        });
    }
    groups
}

async fn rows<T>(txn: &Transaction, store: &str) -> anyhow::Result<Vec<(String, T)>>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync,
{
    Ok(txn
        .get_store::<Table<T>>(store)
        .await?
        .search(|_| true)
        .await?)
}

pub(super) async fn entry_rows(
    txn: &Transaction,
    store_name: &str,
) -> anyhow::Result<Vec<(TurnRequestId, SessionEntry)>> {
    let mut rows = rows::<SessionEntry>(txn, store_name).await?;
    rows.sort_by(|(left_id, left), (right_id, right)| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left_id.cmp(right_id))
    });
    Ok(rows
        .into_iter()
        .map(|(id, entry)| {
            match &entry.entry_type {
                EntryType::Unknown { kind, .. } => {
                    warn!(entry_id = %id, kind = %crate::util::truncate_chars(kind, 80),
                    "Ignoring unknown session entry kind")
                }
                EntryType::BridgeEvent if entry.bridge_role().is_none() => {
                    let detail = entry.bridge_event().map(|event| {
                        format!(
                            "version={} role={}",
                            event.version,
                            crate::util::truncate_chars(&event.role, 80)
                        )
                    });
                    warn!(entry_id = %id, detail = ?detail, "Ignoring unsupported bridge event");
                }
                _ => {}
            }
            (TurnRequestId::parse(id), entry)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ConversationId;
    use eidetica::backend::VerificationStatus;

    #[tokio::test]
    async fn history_refuses_unverified_and_foreign_frontiers() {
        let (instance, mut user, db) = super::super::test_helpers::test_session_db().await;
        let accepted = db.snapshot().await.unwrap();
        let mut session = Session::try_new(ConversationId("reader".into()), db.clone())
            .await
            .unwrap();
        session
            .add_entry(SessionEntry {
                sender: "user".into(),
                content: "not in the accepted prefix".into(),
                timestamp: chrono::Utc::now(),
                entry_type: EntryType::Message,
                routing: None,
                metadata: None,
            })
            .await
            .unwrap();
        let rejected = db.snapshot().await.unwrap();
        let tip = &rejected.tips()[0];
        instance
            .demote_to_unverified(db.root_id(), tip)
            .await
            .unwrap();
        let diagnostic = db.clone().allow_unverified();
        assert!(Session::history_at(&diagnostic, &rejected).await.is_err());
        let reader = Session::history(&diagnostic).await.unwrap();
        assert_eq!(reader.snapshot(), &accepted);
        assert!(reader.entries().await.unwrap().is_empty());
        assert_eq!(
            instance
                .backend()
                .local_engine()
                .unwrap()
                .get_verification_status(tip)
                .await
                .unwrap(),
            VerificationStatus::Unverified
        );

        let key = user.get_default_key().unwrap();
        let other = user
            .create_database(eidetica::crdt::Doc::new(), &key)
            .await
            .unwrap();
        assert!(
            Session::history_at(&db, &other.snapshot().await.unwrap())
                .await
                .is_err()
        );
    }
}
