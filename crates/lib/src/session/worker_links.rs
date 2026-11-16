//! Optional display references. These records never authorize or restart execution.
use super::{
    EntryType, SessionEntry, TurnAttempt, TurnRequestId, TurnTranscriptMessage,
    TurnTranscriptRecord,
};
use eidetica::{Database, store::Table, transaction::Transaction};
use serde::{Deserialize, Serialize};

const SPAWNS: &str = "worker_spawns_v1";
const ORIGIN: &str = "worker_origin_v1";
const OUTCOMES: &str = "worker_outcomes_v1";
const VERSION: u32 = 1;

/// Native tool identity scoped by the parent database, not a provider ID alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCallSite {
    pub request_id: TurnRequestId,
    pub attempt_id: String,
    pub execution_id: Option<String>,
    pub agent: String,
    pub model_sequence: u64,
    pub call_index: usize,
    pub call_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSpawn {
    pub version: u32,
    pub spawn_id: String,
    pub call: WorkerCallSite,
    pub child_session_id: String,
    pub directive_id: TurnRequestId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerOrigin {
    pub version: u32,
    pub parent_session_id: String,
    pub spawn_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTerminal {
    Reply,
    Failed,
    Cancelled,
    NoReply,
}

/// No copied result body. The final row is committed with this reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerOutcome {
    pub version: u32,
    pub directive_id: TurnRequestId,
    pub attempt_id: String,
    pub execution_id: Option<String>,
    pub terminal: WorkerTerminal,
    pub final_entry_id: Option<TurnRequestId>,
}

fn require_version(version: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        version == VERSION,
        "unsupported Worker reference version {version}"
    );
    Ok(())
}

pub async fn worker_spawns(db: &Database) -> anyhow::Result<Vec<WorkerSpawn>> {
    spawns_in_txn(&db.new_transaction().await?).await
}

async fn spawns_in_txn(txn: &Transaction) -> anyhow::Result<Vec<WorkerSpawn>> {
    let rows = txn
        .get_store::<Table<WorkerSpawn>>(SPAWNS)
        .await?
        .search(|_| true)
        .await?;
    let mut spawns = Vec::new();
    for (key, spawn) in rows {
        require_version(spawn.version)?;
        anyhow::ensure!(key == spawn.spawn_id, "Worker spawn key mismatch");
        spawns.push(spawn);
    }
    spawns.sort_by(|a, b| a.spawn_id.cmp(&b.spawn_id));
    Ok(spawns)
}

async fn origin_in_txn(txn: &Transaction) -> anyhow::Result<Option<WorkerOrigin>> {
    let origin: Option<WorkerOrigin> = optional_row(txn, ORIGIN, "origin").await?;
    if let Some(origin) = &origin {
        require_version(origin.version)?;
        anyhow::ensure!(!origin.spawn_id.is_empty(), "empty Worker spawn identity");
        eidetica::entry::ID::parse(&origin.parent_session_id)?;
    }
    Ok(origin)
}

pub async fn worker_origin(db: &Database) -> anyhow::Result<Option<WorkerOrigin>> {
    origin_in_txn(&db.new_transaction().await?).await
}

async fn optional_row<
    T: serde::de::DeserializeOwned + Serialize + Clone + Send + Sync + 'static,
>(
    txn: &Transaction,
    store: &str,
    key: &str,
) -> anyhow::Result<Option<T>> {
    match txn.get_store::<Table<T>>(store).await?.get(key).await {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn insert_once<
    T: serde::de::DeserializeOwned + Serialize + Clone + PartialEq + Send + Sync + 'static,
>(
    txn: &Transaction,
    store: &str,
    key: &str,
    value: T,
) -> anyhow::Result<()> {
    if let Some(old) = optional_row::<T>(txn, store, key).await? {
        anyhow::ensure!(old == value, "immutable Worker reference differs");
    } else {
        txn.get_store::<Table<T>>(store)
            .await?
            .set(key, value)
            .await?;
    }
    Ok(())
}

/// Preparation is unindexed; no callback or execution authority is installed here.
pub(crate) async fn prepare_directive(
    child: &Database,
    parent: &Database,
    spawn_id: &str,
    directive: SessionEntry,
    linked: bool,
) -> anyhow::Result<TurnRequestId> {
    anyhow::ensure!(
        directive.entry_type == EntryType::Directive,
        "Worker requires a Directive"
    );
    let txn = child.new_transaction().await?;
    let id = txn
        .get_store::<Table<SessionEntry>>("entries")
        .await?
        .insert(directive)
        .await?;
    if linked {
        insert_once(
            &txn,
            ORIGIN,
            "origin",
            WorkerOrigin {
                version: VERSION,
                parent_session_id: parent.root_id().to_string(),
                spawn_id: spawn_id.into(),
            },
        )
        .await?;
    }
    txn.commit().await?;
    Ok(TurnRequestId::parse(id))
}

pub(crate) async fn publish_spawn(
    parent: &Database,
    child: &Database,
    spawn_id: &str,
    directive_id: TurnRequestId,
    call: WorkerCallSite,
) -> anyhow::Result<()> {
    let spawn = WorkerSpawn {
        version: VERSION,
        spawn_id: spawn_id.into(),
        call,
        child_session_id: child.root_id().to_string(),
        directive_id,
    };
    let txn = parent.new_transaction().await?;
    anyhow::ensure!(
        spawns_in_txn(&txn)
            .await?
            .iter()
            .all(|old| old.spawn_id == spawn_id || old.call != spawn.call),
        "Worker invocation already references another child"
    );
    // Validate the exact recorded invocation in the same parent snapshot that
    // publishes the reference, not an opaque runtime key or a later turn.
    check_worker_spawn(parent, &txn, child, &spawn).await?;
    insert_once(&txn, SPAWNS, spawn_id, spawn).await?;
    txn.commit().await?;
    Ok(())
}

/// Both handles must already be authorized. This never opens, syncs or grants a DB.
/// Returns the canonical submitted Directive; parent previews are only requests.
pub async fn verify_worker_spawn(
    parent: &Database,
    child: &Database,
    spawn: &WorkerSpawn,
) -> anyhow::Result<SessionEntry> {
    let txn = parent.new_transaction().await?;
    let recorded: Option<WorkerSpawn> = optional_row(&txn, SPAWNS, &spawn.spawn_id).await?;
    anyhow::ensure!(
        recorded.as_ref() == Some(spawn),
        "Worker relation not recorded in parent"
    );
    check_worker_spawn(parent, &txn, child, spawn).await
}

async fn check_worker_spawn(
    parent: &Database,
    parent_txn: &Transaction,
    child: &Database,
    spawn: &WorkerSpawn,
) -> anyhow::Result<SessionEntry> {
    require_version(spawn.version)?;
    anyhow::ensure!(
        spawn.child_session_id == child.root_id().to_string(),
        "Worker child mismatch"
    );
    anyhow::ensure!(
        parent.root_id() != child.root_id(),
        "Worker cannot parent itself"
    );
    let child_txn = child.new_transaction().await?;
    let origin = origin_in_txn(&child_txn)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Worker origin missing"))?;
    anyhow::ensure!(
        origin.parent_session_id == parent.root_id().to_string()
            && origin.spawn_id == spawn.spawn_id,
        "Worker origin mismatch"
    );
    let directive = child_txn
        .get_store::<Table<SessionEntry>>("entries")
        .await?
        .get(spawn.directive_id.as_str())
        .await?;
    anyhow::ensure!(
        directive.entry_type == EntryType::Directive,
        "Worker target is not a Directive"
    );
    let records = parent_txn
        .get_store::<Table<TurnTranscriptRecord>>(super::TURN_TRANSCRIPT_STORE)
        .await?
        .search(|r| r.request_id == spawn.call.request_id && r.attempt_id == spawn.call.attempt_id)
        .await?;
    let matches = records.into_iter().filter(|(_, r)| {
        r.identity.as_ref().is_some_and(|identity| identity.agent == spawn.call.agent && identity.execution_id == spawn.call.execution_id)
            && matches!(&r.message, TurnTranscriptMessage::ModelResponse { model_sequence, tool_calls, .. }
                if *model_sequence == spawn.call.model_sequence
                && tool_calls.get(spawn.call.call_index).is_some_and(|call| call.id == spawn.call.call_id && call.name == "spawn_worker"))
    }).count();
    anyhow::ensure!(
        matches == 1,
        "Worker requires one exact native spawning call"
    );
    Ok(directive)
}

fn outcome_key(attempt: &str, execution: Option<&str>) -> String {
    // JSON tuple avoids delimiter collisions between opaque native identifiers.
    serde_json::to_string(&(attempt, execution)).expect("string tuple is serializable")
}

/// Called only inside completion's transaction, after inserting its final entry.
pub(crate) async fn complete_in_txn(
    txn: &Transaction,
    attempt: &TurnAttempt,
    execution: Option<&str>,
    final_entry: Option<(&TurnRequestId, &SessionEntry)>,
    transcript: Option<&TurnTranscriptRecord>,
) -> anyhow::Result<()> {
    if origin_in_txn(txn).await?.is_none() {
        return Ok(());
    }
    // Each reference names its Directive and native attempt. A later request
    // cannot overwrite the initial spawn's result.
    let entries = txn.get_store::<Table<SessionEntry>>("entries").await?;
    let request = entries.get(attempt.request_id.as_str()).await?;
    if request.entry_type != EntryType::Directive {
        return Ok(());
    }
    if let Some(record) = transcript {
        anyhow::ensure!(
            record.request_id == attempt.request_id
                && record.attempt_id == attempt.attempt_id
                && record
                    .identity
                    .as_ref()
                    .is_none_or(|identity| identity.execution_id.as_deref() == execution),
            "Worker terminal transcript identity mismatch"
        );
    }
    let terminal = if transcript.is_some_and(|r| r.message == TurnTranscriptMessage::Cancelled) {
        anyhow::ensure!(
            final_entry.is_some_and(|(_, entry)| entry.entry_type == EntryType::Error),
            "Worker cancellation requires its acknowledged final entry"
        );
        WorkerTerminal::Cancelled
    } else {
        match final_entry.map(|(_, e)| &e.entry_type) {
            Some(EntryType::Message) => WorkerTerminal::Reply,
            Some(EntryType::Error) => WorkerTerminal::Failed,
            None => WorkerTerminal::NoReply,
            _ => anyhow::bail!("invalid Worker terminal entry"),
        }
    };
    let key = outcome_key(&attempt.attempt_id, execution);
    // Completion is not replayable: even an identical-looking new final has a
    // different canonical row. Preserve the first committed reference.
    anyhow::ensure!(
        optional_row::<WorkerOutcome>(txn, OUTCOMES, &key)
            .await?
            .is_none(),
        "Worker outcome already recorded"
    );
    insert_once(
        txn,
        OUTCOMES,
        &key,
        WorkerOutcome {
            version: VERSION,
            directive_id: attempt.request_id.clone(),
            attempt_id: attempt.attempt_id.clone(),
            execution_id: execution.map(str::to_string),
            terminal,
            final_entry_id: final_entry.map(|(id, _)| id.clone()),
        },
    )
    .await
}

/// Coherent outcome and final-entry read. Missing/invalid references are errors,
/// never empty success. The caller chooses the native attempt/branch explicitly.
pub async fn worker_outcome(
    db: &Database,
    attempt: &str,
    execution: Option<&str>,
) -> anyhow::Result<Option<(WorkerOutcome, Option<SessionEntry>)>> {
    worker_outcome_in_txn(&db.new_transaction().await?, attempt, execution).await
}

/// Select by native attempt generation, not row order, timestamps or latest text.
/// Conflicting executions remain unavailable rather than inventing a winner.
pub async fn latest_worker_outcome(
    db: &Database,
    directive: &TurnRequestId,
) -> anyhow::Result<Option<(WorkerOutcome, Option<SessionEntry>)>> {
    let txn = db.new_transaction().await?;
    let attempts = txn
        .get_store::<Table<TurnAttempt>>(super::TURN_ATTEMPTS_STORE)
        .await?
        .search(|attempt| attempt.request_id == *directive)
        .await?;
    let Some(generation) = attempts.iter().map(|(_, a)| a.generation).max() else {
        return Ok(None);
    };
    let selected: Vec<_> = attempts
        .into_iter()
        .filter(|(_, a)| a.generation == generation)
        .collect();
    anyhow::ensure!(selected.len() == 1, "conflicting Worker attempts");
    let attempt = &selected[0].1;
    if attempt.status != super::TurnAttemptStatus::Completed {
        return Ok(None);
    }
    let outcomes = txn
        .get_store::<Table<WorkerOutcome>>(OUTCOMES)
        .await?
        .search(|outcome| {
            outcome.attempt_id == attempt.attempt_id && outcome.directive_id == *directive
        })
        .await?;
    anyhow::ensure!(outcomes.len() <= 1, "conflicting Worker execution outcomes");
    let Some((_, outcome)) = outcomes.first() else {
        return Ok(None);
    };
    worker_outcome_in_txn(&txn, &attempt.attempt_id, outcome.execution_id.as_deref()).await
}

async fn worker_outcome_in_txn(
    txn: &Transaction,
    attempt: &str,
    execution: Option<&str>,
) -> anyhow::Result<Option<(WorkerOutcome, Option<SessionEntry>)>> {
    let Some(outcome): Option<WorkerOutcome> =
        optional_row(txn, OUTCOMES, &outcome_key(attempt, execution)).await?
    else {
        return Ok(None);
    };
    require_version(outcome.version)?;
    anyhow::ensure!(
        origin_in_txn(txn).await?.is_some(),
        "Worker outcome has no origin"
    );
    anyhow::ensure!(
        outcome.attempt_id == attempt && outcome.execution_id.as_deref() == execution,
        "Worker outcome identity mismatch"
    );
    let completed = txn
        .get_store::<Table<TurnAttempt>>(super::TURN_ATTEMPTS_STORE)
        .await?
        .get(attempt)
        .await?;
    anyhow::ensure!(
        completed.attempt_id == attempt
            && completed.request_id == outcome.directive_id
            && completed.status == super::TurnAttemptStatus::Completed,
        "Worker completion reference mismatch"
    );
    let entry = match &outcome.final_entry_id {
        Some(id) => Some(
            txn.get_store::<Table<SessionEntry>>("entries")
                .await?
                .get(id.as_str())
                .await?,
        ),
        None => None,
    };
    let valid = matches!(
        (&outcome.terminal, entry.as_ref().map(|e| &e.entry_type)),
        (WorkerTerminal::Reply, Some(EntryType::Message))
            | (
                WorkerTerminal::Failed | WorkerTerminal::Cancelled,
                Some(EntryType::Error)
            )
            | (WorkerTerminal::NoReply, None)
    );
    anyhow::ensure!(valid, "Worker terminal reference mismatch");
    Ok(Some((outcome, entry)))
}

#[cfg(test)]
mod tests;
