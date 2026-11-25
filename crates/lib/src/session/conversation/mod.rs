//! Only Session's authorized history handle selects a format. ConversationStore
//! validates and interprets immutable data; it does not commit or execute it.
mod atom;
mod compaction;
pub(super) mod contract;
mod legacy;
mod provenance;
mod record;
mod turn;
mod view;

pub(super) mod staging;
#[cfg(test)]
mod tests;

pub use provenance::{CallInput, CallMessageBinding, Fidelity, RecordRef};
pub use record::{CallId, FactId, ObservedBinding, RepresentationId};

use crate::session::{SessionContextView, history::DisplayRow};
use eidetica::{
    Database, Snapshot, Transaction,
    entry::{Entry, ID},
};
use record::Operation;
use std::collections::{BTreeMap, BTreeSet};

const STORE: &str = "conversation_v1";

type Result<T> = std::result::Result<T, ConversationError>;

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ConversationError {
    #[error("conversation integrity: {0}")]
    Integrity(String),
    #[error("malformed conversation: {0}")]
    Malformed(String),
    #[error("unsupported conversation: {0}")]
    Unsupported(String),
    #[error("missing conversation reference: {0}")]
    Missing(String),
    #[error("invalid conversation scope: {0}")]
    InvalidScope(String),
    #[error("conversation identity conflict: {0}")]
    IdentityConflict(String),
    #[error("overlapping conversation replacement: {0:?}")]
    ReplacementConflict(RepresentationId),
    #[error("conversation exchange conflict: {0}")]
    ExchangeConflict(String),
    #[error("conversation storage: {0}")]
    Storage(String),
}

impl From<eidetica::Error> for ConversationError {
    fn from(error: eidetica::Error) -> Self {
        anyhow::Error::from(error).into()
    }
}
impl From<anyhow::Error> for ConversationError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(format!("{error:#}"))
    }
}
impl From<serde_json::Error> for ConversationError {
    fn from(error: serde_json::Error) -> Self {
        Self::Malformed(error.to_string())
    }
}

// Keep the exact Store identities across readers and writers.
macro_rules! doc_store {
    ($store:ident, $kind:literal) => {
        impl eidetica::Registered for $store {
            fn type_id() -> &'static str {
                $kind
            }
        }
        #[async_trait::async_trait]
        impl eidetica::Store for $store {
            type Data = eidetica::crdt::Doc;
            async fn load(txn: &Transaction, name: String) -> eidetica::Result<Self> {
                Ok(Self(eidetica::store::DocStore::load(txn, name).await?))
            }
            fn name(&self) -> &str {
                self.0.name()
            }
            fn transaction(&self) -> &Transaction {
                self.0.transaction()
            }
        }
        impl $crate::session::conversation::ReadAtoms for $store {
            fn doc(&self) -> &eidetica::store::DocStore {
                &self.0
            }
        }
    };
}
use doc_store;

struct ConversationStore(eidetica::store::DocStore);
doc_store!(ConversationStore, "chaz/conversation:v1");

trait ReadAtoms: eidetica::Store + Send {
    fn doc(&self) -> &eidetica::store::DocStore;
}

/// Byte-preserving inspection. Invalid deltas are keyed by signed Entry ID;
/// an undecodable atom in a well-formed delta is retained in `atoms`.
#[derive(Clone, Debug, Default)]
pub struct ConversationOriginals {
    pub atoms: BTreeMap<String, String>,
    pub invalid_deltas: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub struct ConversationInspection {
    pub descriptor: ConversationOriginals,
    pub operations: ConversationOriginals,
    pub provenance: ConversationOriginals,
    pub issues: Vec<ConversationError>,
}

#[derive(Default)]
struct Originals {
    data: ConversationOriginals,
    issues: Vec<ConversationError>,
}

impl Originals {
    fn delta(&mut self, entry: &Entry, store: &str) -> Result<BTreeMap<String, String>> {
        let bytes = entry.data(store)?;
        match atom::decode_doc(bytes).and_then(|doc| atom::atoms(&doc)) {
            Ok(atoms) => {
                for (hash, text) in &atoms {
                    if self
                        .data
                        .atoms
                        .insert(hash.clone(), text.clone())
                        .is_some_and(|old| old != *text)
                    {
                        self.issues
                            .push(ConversationError::Integrity("atom hash collision".into()));
                    }
                }
                Ok(atoms)
            }
            Err(error) => {
                self.data
                    .invalid_deltas
                    .insert(entry.id().to_string(), bytes.to_vec());
                self.issues.push(error);
                Ok(BTreeMap::new())
            }
        }
    }

    async fn check_materialized<S: ReadAtoms>(&self, txn: &Transaction, name: &str) -> Result<()> {
        if !self.data.atoms.is_empty() {
            // get_store can register an absent Store in a scratch transaction;
            // it must not cover up missing registration for authored atoms.
            txn.get_index().await?.get_entry(name).await?;
        }
        let store = txn.get_store::<S>(name).await?;
        if atom::atoms(&store.doc().get_all().await?)? != self.data.atoms {
            return Err(ConversationError::Integrity(format!(
                "{name} materialization differs from immutable atom union"
            )));
        }
        Ok(())
    }
}

pub(super) struct ConversationRead {
    inspection: ConversationInspection,
    legacy: legacy::LegacyView,
    fenced_turn: bool,
    fenced_compaction: bool,
    fenced_input_appends: bool,
    legacy_frontier: Option<Snapshot>,
    descriptor_entries: BTreeSet<ID>,
    locations: BTreeMap<ID, BTreeSet<String>>,
    operations: BTreeMap<String, Operation>,
    op_atoms: BTreeMap<String, String>,
    provenance: provenance::Evidence,
}

impl ConversationRead {
    pub(super) fn inspection(&self) -> &ConversationInspection {
        &self.inspection
    }

    fn effective(&self) -> Result<view::ConversationView> {
        if let Some(error) = self.inspection.issues.first() {
            return Err(error.clone());
        }
        view::effective(&self.operations, &self.legacy)
    }

    pub(super) fn context(&self) -> Result<SessionContextView> {
        Ok(SessionContextView {
            groups: self.effective()?.context_groups()?,
            contributions: self.legacy.contributions.clone(),
            read_only: true,
            receipts: self.legacy.receipts.clone(),
            #[cfg(test)]
            read_volume: serde_json::Value::Null,
        })
    }

    pub(super) fn display_rows(&self) -> Result<Vec<DisplayRow>> {
        Ok(self
            .effective()?
            .display_rows(&self.provenance.reply_targets))
    }

    pub(super) fn recorded_call(&self, call: &CallId, attempt: &str) -> Result<Option<&CallInput>> {
        if let Some(error) = self.inspection.issues.first() {
            return Err(error.clone());
        }
        // Even if later concurrent replacements make the current view
        // ambiguous, an independently validated call input is still evidence.
        Ok(self
            .provenance
            .calls
            .get(&(call.clone(), attempt.to_owned())))
    }

    pub(super) fn accounting(&self) -> Result<Vec<super::usage::CallEvidence>> {
        if let Some(error) = self.inspection.issues.first() {
            return Err(error.clone());
        }
        Ok(self.provenance.accounting.clone())
    }

    pub(super) fn legacy_frontier(&self) -> Result<&Snapshot> {
        if let Some(error) = self.inspection.issues.first() {
            return Err(error.clone());
        }
        self.legacy_frontier
            .as_ref()
            .ok_or_else(|| ConversationError::Missing("legacy boundary".into()))
    }

    fn require_descriptor(&self, ancestors: &BTreeSet<ID>) -> Result<()> {
        if self.descriptor_entries.is_disjoint(ancestors) {
            return Err(ConversationError::InvalidScope(
                "record did not observe the session contract".into(),
            ));
        }
        Ok(())
    }

    fn operations_in(&self, ancestors: &BTreeSet<ID>) -> BTreeMap<String, Operation> {
        let selected: BTreeSet<_> = ancestors
            .iter()
            .filter_map(|id| self.locations.get(id))
            .flatten()
            .collect();
        selected
            .into_iter()
            .map(|id| (id.clone(), self.operations[id].clone()))
            .collect()
    }
}

/// Read signed Entry bodies only through the caller's Database handle, not the
/// raw local engine or connection-login identity.
async fn entries(db: &Database, name: &str, frontier: &Snapshot) -> Result<Vec<Entry>> {
    Ok(super::execution::store_entries_at(db, name, frontier).await?)
}

/// Per-read main ancestry. All tests of observed parents share these immutable
/// entries; a long prefix is not fetched anew for each atom/provenance record.
struct Ancestry(BTreeMap<ID, Vec<ID>>);

impl Ancestry {
    async fn load(db: &Database, frontier: &Snapshot) -> Result<Self> {
        let mut entries = BTreeMap::new();
        let mut pending = frontier.tips().to_vec();
        while let Some(id) = pending.pop() {
            if let std::collections::btree_map::Entry::Vacant(slot) = entries.entry(id) {
                let entry = db.get_entry(slot.key()).await?;
                if !entry.in_tree(db.root_id()) {
                    return Err(ConversationError::InvalidScope("foreign ancestry".into()));
                }
                let parents = entry.parents()?;
                pending.extend(parents.iter().cloned());
                slot.insert(parents);
            }
        }
        Ok(Self(entries))
    }

    fn prefix(&self, frontier: &Snapshot) -> Result<BTreeSet<ID>> {
        let mut seen = BTreeSet::new();
        let mut pending = frontier.tips().to_vec();
        while let Some(id) = pending.pop() {
            if seen.insert(id.clone()) {
                let parents = self.0.get(&id).ok_or_else(|| {
                    ConversationError::InvalidScope("tip outside selected ancestry".into())
                })?;
                pending.extend(parents.iter().cloned());
            }
        }
        Ok(seen)
    }
}

pub(super) async fn open(
    db: &Database,
    frontier: &Snapshot,
    txn: &Transaction,
) -> Result<ConversationRead> {
    let ancestry = Ancestry::load(db, frontier).await?;
    let (descriptor, descriptor_entries, mut contract) =
        contract::inspect_at(db, frontier, txn, &ancestry).await?;
    let fenced_turn = descriptor
        .as_ref()
        .is_some_and(contract::Descriptor::fenced_turn);
    let fenced_compaction = descriptor
        .as_ref()
        .is_some_and(contract::Descriptor::fenced_compaction);
    let fenced_input_appends = descriptor
        .as_ref()
        .is_some_and(contract::Descriptor::fenced_input_appends);
    let legacy_frontier = descriptor.as_ref().map(|d| d.legacy.frontier.clone());
    let legacy = if let Some(descriptor) = descriptor.filter(|_| contract.issues.is_empty()) {
        match contract::require_unchanged_legacy(db, frontier, &descriptor.legacy, &ancestry).await
        {
            Ok(()) => match legacy::at(db, &descriptor.legacy).await {
                Ok(legacy) => legacy,
                Err(error) => {
                    contract.issues.push(error);
                    legacy::LegacyView::default()
                }
            },
            Err(error) => {
                contract.issues.push(error);
                legacy::LegacyView::default()
            }
        }
    } else {
        if contract.issues.is_empty() {
            contract.issues.push(ConversationError::Unsupported(
                "v1 Store traces without a valid descriptor".into(),
            ));
        }
        legacy::LegacyView::default()
    };
    let mut read = ConversationRead {
        inspection: ConversationInspection {
            descriptor: contract.data,
            issues: contract.issues,
            ..Default::default()
        },
        legacy,
        fenced_turn,
        fenced_compaction,
        fenced_input_appends,
        legacy_frontier,
        descriptor_entries,
        locations: BTreeMap::new(),
        operations: BTreeMap::new(),
        op_atoms: BTreeMap::new(),
        provenance: Default::default(),
    };
    let entries = entries(db, STORE, frontier).await?;
    let mut originals = Originals::default();
    let mut occurrences = Vec::new();
    for entry in &entries {
        for (hash, text) in originals.delta(entry, STORE)? {
            match atom::decode_v1::<Operation>(&text) {
                Ok(op) => {
                    if read
                        .op_atoms
                        .insert(op.op_id.clone(), hash.clone())
                        .is_some_and(|old| old != hash)
                    {
                        originals
                            .issues
                            .push(ConversationError::IdentityConflict(op.op_id.clone()));
                    }
                    read.locations
                        .entry(entry.id())
                        .or_default()
                        .insert(op.op_id.clone());
                    occurrences.push((entry, op.op_id.clone()));
                    read.operations.insert(op.op_id.clone(), op);
                }
                Err(error) => originals.issues.push(error),
            }
        }
    }
    if let Err(error) = originals
        .check_materialized::<ConversationStore>(txn, STORE)
        .await
    {
        originals.issues.push(error);
    }
    read.inspection.operations = originals.data;
    read.inspection.issues.extend(originals.issues);
    if read.inspection.issues.is_empty() {
        for (entry, id) in occurrences {
            let op = &read.operations[&id];
            let check = async {
                let parents = ancestry.prefix(&Snapshot::from(entry.parents()?))?;
                if op.observed.database != db.root_id().to_string()
                    || !op
                        .observed
                        .frontier
                        .tips()
                        .iter()
                        .all(|tip| parents.contains(tip))
                {
                    return Err(ConversationError::InvalidScope(
                        "operation binding outside Entry ancestry".into(),
                    ));
                }
                let observed = ancestry.prefix(&op.observed.frontier)?;
                let subset = read.operations_in(&observed);
                // Byte-identical reimport is not a new event; the original
                // occurrence still has to pass signed-boundary validation.
                if !subset.contains_key(&id) {
                    read.require_descriptor(&observed)?;
                    view::validate_at_observed(op, &subset, &read.legacy)?;
                }
                Ok(())
            }
            .await;
            if let Err(error) = check {
                read.inspection.issues.push(error);
            }
        }
    }
    let (provenance, evidence) =
        Box::pin(provenance::read(db, frontier, txn, &read, &ancestry)).await?;
    read.inspection.provenance = provenance.data;
    read.inspection.issues.extend(provenance.issues);
    read.provenance = evidence;
    Ok(read)
}
