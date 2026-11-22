//! Reader-first session contract. There is deliberately no production activation
//! API: a descriptor records semantics, not proof of the operational writer barrier.
use eidetica::{Database, Snapshot, Transaction, entry::ID, store::DocStore};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use super::{Ancestry, ConversationError, Originals, Result, atom};

pub(super) const STORE: &str = "conversation_contract_v1";
pub(super) const FENCED_TURN: &str = "fenced-turn-v1";
pub(super) const FENCED_COMPACTION: &str = "fenced-compaction-v1";
pub(super) const FENCED_INPUT_APPENDS: &str = "fenced-input-appends-v1";

/// Context-owned legacy semantic stores covered by this reader increment.
/// Job/input/auxiliary conversion and full write-door integration remain pending;
/// this list is not an all-writers activation barrier. Authority is not frozen.
const LEGACY_STORES: &[&str] = &[
    "entries",
    "turn_transcript",
    "execution_transcript_v1",
    "session_commands",
    "session_command_results",
    "context_contributions",
    "context_strategy_state",
    "call_telemetry",
    "context_selections",
    "job_inputs",
    "job_input_receipts",
    "job_input_closed",
    "job_input_calls",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyBoundary {
    pub database: String,
    /// Main DAG frontier; anchors never use a moving current head.
    pub frontier: Snapshot,
    pub adapter_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub version: u32,
    pub encoding: u32,
    pub required_operations: BTreeSet<String>,
    pub context_semantics: u32,
    pub renderer: u32,
    pub legacy: LegacyBoundary,
}

impl Descriptor {
    pub fn require_supported(&self) -> Result<()> {
        if self.version != 1
            || self.encoding != 1
            || self.context_semantics != 1
            || self.renderer != 1
            || self.legacy.adapter_version != 1
        {
            return Err(ConversationError::Unsupported(
                "session conversation contract version".into(),
            ));
        }
        let mut supported = BTreeSet::from(["append-v1".into(), "replace-v1".into()]);
        if self.fenced_turn() {
            supported.insert(FENCED_TURN.into());
            if self.fenced_compaction() {
                supported.insert(FENCED_COMPACTION.into());
            } else if self.fenced_input_appends() {
                supported.insert(FENCED_INPUT_APPENDS.into());
            }
        }
        if self.required_operations != supported {
            return Err(ConversationError::Unsupported(
                "required conversation operation capabilities".into(),
            ));
        }
        if self.legacy.database.is_empty() || self.legacy.frontier.is_empty() {
            return Err(ConversationError::InvalidScope("legacy boundary".into()));
        }
        Ok(())
    }

    pub(super) fn fenced_compaction(&self) -> bool {
        self.required_operations.contains(FENCED_COMPACTION)
    }

    pub(super) fn fenced_turn(&self) -> bool {
        self.required_operations.contains(FENCED_TURN)
    }

    pub(super) fn fenced_input_appends(&self) -> bool {
        self.required_operations.contains(FENCED_INPUT_APPENDS)
    }
}

pub(super) struct ContractStore(pub(super) DocStore);
super::doc_store!(ContractStore, "chaz/conversation-contract:v1");

/// A removal or overwrite cannot make an authored descriptor disappear.
pub(super) async fn inspect_at(
    db: &Database,
    frontier: &Snapshot,
    txn: &Transaction,
    ancestry: &Ancestry,
) -> Result<(Option<Descriptor>, BTreeSet<ID>, Originals)> {
    let entries = super::entries(db, STORE, frontier).await?;
    let mut originals = Originals::default();
    let mut descriptor = None;
    let mut locations = BTreeSet::new();
    for entry in entries {
        for text in originals.delta(&entry, STORE)?.into_values() {
            let check = async {
                let desc: Descriptor = atom::decode_v1(&text)?;
                desc.require_supported()?;
                let parents = ancestry.prefix(&Snapshot::from(entry.parents()?))?;
                if desc.legacy.database != db.root_id().to_string()
                    || !desc
                        .legacy
                        .frontier
                        .tips()
                        .iter()
                        .all(|id| parents.contains(id))
                {
                    return Err(ConversationError::InvalidScope(
                        "activation boundary outside descriptor Entry ancestry".into(),
                    ));
                }
                if desc.fenced_turn() && desc.legacy.frontier.tips() != [db.root_id().clone()] {
                    return Err(ConversationError::InvalidScope(
                        "fenced turn is not on a fresh session".into(),
                    ));
                }
                locations.insert(entry.id());
                descriptor = Some(desc);
                Ok(())
            }
            .await;
            if let Err(error) = check {
                originals.issues.push(error);
            }
        }
    }
    if originals.data.atoms.len() > 1 {
        originals.issues.push(ConversationError::IdentityConflict(
            "multiple session conversation descriptors".into(),
        ));
    }
    if let Err(error) = originals
        .check_materialized::<ContractStore>(txn, STORE)
        .await
    {
        originals.issues.push(error);
    }
    Ok((descriptor, locations, originals))
}

// Registry metadata is checked too: a declared but empty or mistyped Store
// cannot fall back to the legacy format. The accounting reader is not enabled
// in this slice, but its store name is reserved by the v1 contract.
const FORMAT_STORES: &[&str] = &[
    STORE,
    super::STORE,
    super::provenance::STORE,
    "conversation_call_telemetry_v1",
];

pub(crate) async fn has_traces(
    db: &Database,
    frontier: &Snapshot,
    txn: &Transaction,
) -> Result<bool> {
    let index = txn.get_index().await?;
    for store in FORMAT_STORES {
        match index.get_entry(store).await {
            Ok(_) => return Ok(true),
            Err(eidetica::Error::Store(error)) if matches!(error.as_ref(), eidetica::store::StoreError::KeyNotFound { key, .. } if key == store) =>
                {}

            Err(error) => return Err(error.into()),
        }
    }
    // The local lookup is structural only, at Session's accepted frontier;
    // never use the raw backend to fetch bodies or inspect unverified history.
    // A remote handle must do even this probe through its own authenticated key.
    if let Some(engine) = db.backend()?.local_engine() {
        for store in FORMAT_STORES {
            if !engine
                .store_snapshot_at(db.root_id(), store, frontier)
                .await?
                .is_empty()
            {
                return Ok(true);
            }
        }
    } else {
        let names = FORMAT_STORES
            .iter()
            .map(|store| (*store).into())
            .collect::<Vec<_>>();
        let context = db
            .transaction_context(&names, eidetica::service::protocol::ReadScope::Verified)
            .await?;
        let current = Snapshot::from(
            context
                .main_parents
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
        );
        if &current == frontier {
            return Ok(names.iter().any(|name| {
                context
                    .subtree_parents
                    .get(name)
                    .is_none_or(|tips| !tips.is_empty())
            }));
        }
        for store in FORMAT_STORES {
            if !super::super::execution::store_entries_at(db, store, frontier)
                .await?
                .is_empty()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Detect writes in legacy semantic stores outside the pinned prefix, including
/// an unrelated concurrent branch and a same-value write. Effects cannot be
/// undone by this check. Operators must prevent incompatible writers first.
pub(super) async fn require_unchanged_legacy(
    db: &Database,
    frontier: &Snapshot,
    boundary: &LegacyBoundary,
    ancestry: &Ancestry,
) -> Result<()> {
    if boundary.database != db.root_id().to_string() || boundary.frontier.is_empty() {
        return Err(ConversationError::InvalidScope(
            "legacy boundary database/frontier".into(),
        ));
    }
    if boundary.adapter_version != 1 {
        return Err(ConversationError::Unsupported(
            "legacy adapter version".into(),
        ));
    }
    let prefix = ancestry.prefix(&boundary.frontier)?;
    for store in LEGACY_STORES {
        let entries = super::super::execution::store_entries_at(db, store, frontier).await?;
        let later: Vec<_> = entries
            .iter()
            .filter(|entry| !prefix.contains(entry.id_ref()))
            .collect();
        if later.is_empty() {
            continue;
        }
        if *store == "entries" {
            validate_retained_controls(db, boundary, &later).await?;
        } else {
            return Err(ConversationError::InvalidScope(format!(
                "legacy semantic write after conversation boundary in {store}"
            )));
        }
    }
    Ok(())
}

/// Approval, stop and acknowledgement rows are independent safety observations,
/// not model context. Keep their original protocol/body in `entries`; only fresh
/// insertions are compatible after the semantic history boundary.
pub(in crate::session) fn retained_control(entry: &crate::session::SessionEntry) -> bool {
    matches!(
        entry.entry_type,
        crate::session::EntryType::ApprovalRequest
            | crate::session::EntryType::ApprovalDecision
            | crate::session::EntryType::Cancel
            | crate::session::EntryType::Ack
    )
}

async fn validate_retained_controls(
    db: &Database,
    boundary: &LegacyBoundary,
    entries: &[&eidetica::entry::Entry],
) -> Result<()> {
    use eidetica::store::Table;
    let txn = db.new_transaction_at(&boundary.frontier).await?;
    let mut seen: BTreeSet<_> = txn
        .get_store::<Table<crate::session::SessionEntry>>("entries")
        .await?
        .search(|_| true)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for entry in entries {
        // Inspect every signed delta, not just the winning/current Table row:
        // a later repair must not conceal an overwrite, deletion or bad branch.
        let delta = atom::decode_doc(entry.data("entries")?)?;
        let envelope = serde_json::to_value(&delta)?;
        let count = envelope
            .get("children")
            .and_then(serde_json::Value::as_object)
            .map(|children| children.len());
        if delta.is_atomic() || count != Some(delta.len()) {
            return Err(ConversationError::Integrity(
                "atomic retained controls or tombstone".into(),
            ));
        }
        for (id, value) in delta.iter() {
            let Some(text) = value.as_text() else {
                return Err(ConversationError::Integrity(
                    "retained control is not an inserted row".into(),
                ));
            };
            let row: crate::session::SessionEntry = atom::decode(text)?;
            if uuid::Uuid::parse_str(id).is_err()
                || !retained_control(&row)
                || seen.iter().any(|prior| {
                    prior == id
                        || prior
                            .strip_prefix(id)
                            .is_some_and(|tail| tail.starts_with('.'))
                        || id
                            .strip_prefix(prior.as_str())
                            .is_some_and(|tail| tail.starts_with('.'))
                })
            {
                return Err(ConversationError::InvalidScope(
                    "legacy semantic write or reused retained-control identity after conversation boundary in entries".into(),
                ));
            }
            seen.insert(id.clone());
        }
    }
    Ok(())
}
