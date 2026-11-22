//! Read-only history at one accepted main frontier. This is the format boundary.
use super::conversation::{self, CallId, CallInput, ConversationInspection, RepresentationId};
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
use tokio::sync::OnceCell;
use tracing::warn;

/// A Session history read using the caller's database handle (and therefore
/// its native read scope). Never hands out a transaction or installs a writer.
pub struct HistoryRead<'a> {
    database: &'a Database,
    snapshot: Snapshot,
    txn: Transaction,
    format: Format,
}

enum Format {
    Legacy,
    ConversationV1(Box<OnceCell<conversation::ConversationRead>>),
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

/// One context selection unit. Each timeline uses this same ordered list;
/// a native v1 call and its results occupy one indivisible unit.
#[derive(Clone, Debug)]
pub struct HistoryGroup {
    pub(crate) body: HistoryBody,
    pub(crate) representation: Option<RepresentationId>,
}

#[derive(Clone, Debug)]
pub(crate) enum HistoryBody {
    Entry(Box<ContextGroup>),
    Recorded {
        kind: EntryType,
        messages: Vec<(crate::runtime::RuntimeMessage, RepresentationId)>,
    },
}

impl HistoryGroup {
    pub(crate) fn legacy(group: ContextGroup) -> Self {
        Self {
            body: HistoryBody::Entry(Box::new(group)),
            representation: None,
        }
    }

    pub fn entry_group(&self) -> Option<&ContextGroup> {
        match &self.body {
            HistoryBody::Entry(group) => Some(group),
            HistoryBody::Recorded { .. } => None,
        }
    }

    pub fn entry_id(&self) -> Option<&TurnRequestId> {
        self.entry_group()?.entry_id()
    }

    /// The Store representation for this timeline group's first member.
    pub fn representation_id(&self) -> Option<&RepresentationId> {
        self.representation.as_ref()
    }

    pub(crate) fn kind(&self) -> Option<&EntryType> {
        match &self.body {
            HistoryBody::Entry(group) if crate::context::is_context_entry(&group.entry) => {
                Some(&group.entry.entry_type)
            }
            HistoryBody::Entry(_) => None,
            HistoryBody::Recorded { kind, .. } => Some(kind),
        }
    }

    pub(crate) fn date(&self) -> Option<chrono::NaiveDate> {
        self.entry_group()
            .map(|group| group.entry.timestamp.date_naive())
    }

    pub(crate) fn positional_summary(&self) -> bool {
        self.representation.is_none() && self.kind() == Some(&EntryType::Summary)
    }
}

/// Selected history and committed contributions, not a store or execution grant.
#[derive(Clone, Debug)]
pub struct SessionContextView {
    pub groups: Vec<HistoryGroup>,
    pub contributions: Vec<CommittedContribution>,
    // Needs to survive a v1 view with no visible groups (e.g. replaced all rows).
    pub(crate) read_only: bool,
    pub(crate) receipts: Vec<String>,
    #[cfg(test)]
    pub(crate) read_volume: serde_json::Value,
}

impl SessionContextView {
    /// Rows with native legacy entry metadata. Use groups for complete context.
    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &SessionEntry> + '_ {
        self.groups
            .iter()
            .filter_map(|group| Some(&group.entry_group()?.entry))
    }

    pub(super) fn into_flat(
        self,
    ) -> anyhow::Result<(Vec<SessionEntry>, Vec<Vec<TurnTranscriptRecord>>)> {
        anyhow::ensure!(
            !self.read_only,
            "legacy flat transcript cannot represent v1 history"
        );
        Ok(self
            .groups
            .into_iter()
            .filter_map(|group| match group.body {
                HistoryBody::Entry(group) => Some((
                    group.entry,
                    group.transcript.into_iter().map(|row| row.record).collect(),
                )),
                _ => None,
            })
            .unzip())
    }
}

pub(crate) struct DisplayRow {
    pub sender: String,
    pub content: String,
    pub kind: EntryType,
}

impl From<SessionEntry> for DisplayRow {
    fn from(entry: SessionEntry) -> Self {
        Self {
            sender: entry.sender,
            content: entry.content,
            kind: entry.entry_type,
        }
    }
}

impl Session {
    /// Legacy runtime/ingress gate during the reader-first rollout.
    pub(crate) async fn require_legacy_history(database: &Database) -> anyhow::Result<()> {
        Self::history(database).await?.require_legacy()
    }

    pub(super) async fn legacy_history_transaction(
        database: &Database,
    ) -> anyhow::Result<Transaction> {
        // Use the ordinary writer frontier (with normal signer/verification
        // behavior), and check exactly the parents to which this write binds.
        let snapshot = database.snapshot().await?;
        let read = HistoryRead::open(database, snapshot).await?;
        read.require_legacy()?;
        Ok(read.txn)
    }

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
        Self::require_accepted_history_frontier(database, snapshot).await?;
        HistoryRead::open(database, snapshot.clone()).await
    }

    pub(super) async fn require_accepted_history_frontier(
        database: &Database,
        snapshot: &Snapshot,
    ) -> anyhow::Result<()> {
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
        Ok(())
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
        let format = if conversation::contract::has_traces(database, &snapshot, &txn).await? {
            Format::ConversationV1(Box::default())
        } else {
            Format::Legacy
        };
        Ok(HistoryRead {
            database,
            snapshot,
            txn,
            format,
        })
    }

    async fn conversation(&self) -> anyhow::Result<Option<&conversation::ConversationRead>> {
        match &self.format {
            Format::Legacy => Ok(None),
            Format::ConversationV1(read) => Ok(Some(
                read.get_or_try_init(|| async {
                    conversation::open(self.database, &self.snapshot, &self.txn).await
                })
                .await?,
            )),
        }
    }

    pub(super) fn require_legacy(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.format, Format::Legacy),
            "v1 conversation is read-only; this path requires legacy session history"
        );
        Ok(())
    }

    /// Inspect original signed Store atoms, including invalid deltas. No
    /// effective winner, fallback, write, or execution is implied.
    pub async fn inspect_conversation(&self) -> anyhow::Result<Option<&ConversationInspection>> {
        Ok(self.conversation().await?.map(|read| read.inspection()))
    }

    /// Obtain an immutable preparation at this frontier. The Store validates
    /// its selection against the original observed snapshot, not this head.
    pub async fn recorded_call(
        &self,
        call: &CallId,
        attempt: &str,
    ) -> anyhow::Result<Option<&CallInput>> {
        Ok(match self.conversation().await? {
            Some(read) => read.recorded_call(call, attempt)?,
            None => None,
        })
    }

    pub(super) async fn conversation_accounting(
        &self,
    ) -> anyhow::Result<Vec<super::usage::CallEvidence>> {
        Ok(match self.conversation().await? {
            Some(read) => read.accounting()?,
            None => Vec::new(),
        })
    }

    pub(super) async fn usage_entries(&self) -> anyhow::Result<Vec<SessionEntry>> {
        let rows = match self.conversation().await? {
            Some(read) => {
                let legacy = self
                    .database
                    .new_transaction_at(read.legacy_frontier()?)
                    .await?;
                entry_rows(&legacy, "entries").await?
            }
            None => self.entries().await?,
        };
        Ok(rows.into_iter().map(|(_, entry)| entry).collect())
    }

    pub(crate) async fn display_rows(&self) -> anyhow::Result<Vec<DisplayRow>> {
        Ok(match self.conversation().await? {
            Some(read) => read.display_rows()?,
            None => self
                .entries()
                .await?
                .into_iter()
                .map(|(_, row)| row.into())
                .collect(),
        })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Dated legacy rows, before compaction. This API also feeds old transport
    /// delivery/queue readers and must not pass v1 projections as raw rows.
    pub async fn entries(&self) -> anyhow::Result<Vec<(TurnRequestId, SessionEntry)>> {
        self.require_legacy()?;
        entry_rows(&self.txn, "entries").await
    }

    pub async fn context(&self) -> anyhow::Result<SessionContextView> {
        match self.conversation().await? {
            Some(read) => Ok(read.context()?),
            None => self.context_for(None, &|_| true).await,
        }
    }

    pub async fn turn_context(
        &self,
        current: &TurnRequestId,
        is_agent: impl Fn(&str) -> bool + Sync,
    ) -> anyhow::Result<SessionContextView> {
        self.require_legacy()?;
        self.context_for(Some(current), &is_agent).await
    }

    pub(super) async fn context_entries(&self) -> anyhow::Result<Vec<SessionEntry>> {
        self.require_legacy()?;
        Ok(self
            .compacted_groups(self.entries().await?, &|_, _| false)
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
        entries: Vec<(TurnRequestId, SessionEntry)>,
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
        groups.extend(entries.into_iter().filter_map(|(id, entry)| {
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
        let (groups, contributions, _volume) = self
            .legacy_context(self.entries().await?, current, is_agent)
            .await?;
        Ok(SessionContextView {
            groups: groups.into_iter().map(HistoryGroup::legacy).collect(),
            contributions,
            read_only: false,
            receipts: Vec::new(),
            #[cfg(test)]
            read_volume: _volume,
        })
    }

    pub(super) async fn legacy_context(
        &self,
        entries: Vec<(TurnRequestId, SessionEntry)>,
        current: Option<&TurnRequestId>,
        is_agent: &(dyn Fn(&str) -> bool + Sync),
    ) -> anyhow::Result<(Vec<ContextGroup>, Vec<CommittedContribution>, ReadVolume)> {
        self.require_legacy()?;
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
            .compacted_groups(entries, &|id, entry| {
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
        #[cfg(not(test))]
        let read_volume = ();
        Ok((groups, contributions, read_volume))
    }

    /// Retained evidence intentionally bypasses compaction; its scoped caller
    /// still checks current participation and source-tool visibility.
    pub(crate) async fn retained_tool_history(&self) -> anyhow::Result<Vec<Vec<TranscriptRow>>> {
        self.require_legacy()?;
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

#[cfg(test)]
type ReadVolume = serde_json::Value;
#[cfg(not(test))]
type ReadVolume = ();

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
