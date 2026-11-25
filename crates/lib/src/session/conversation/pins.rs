//! Session membership pins. An Entry identifies a pin revision; native main
//! ancestry says which updates supersede which. No last-writer-wins lookup.
use super::{Ancestry, ConversationError as Error, ConversationRead, RecordRef, Result, atom};
use eidetica::{Database, Snapshot, Transaction, entry::ID, store::DocStore};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const STORE: &str = "conversation_agent_pins_v1";

struct PinStore(DocStore);
super::doc_store!(PinStore, "chaz/conversation-agent-pins:v1");

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pin {
    version: u32,
    update: bool,
    config: RecordRef,
}

#[derive(Default)]
pub(super) struct Pins {
    records: BTreeMap<ID, Pin>,
    supersedes: BTreeMap<ID, ID>,
}

impl Pins {
    pub(super) fn config(&self, id: &ID) -> Option<&RecordRef> {
        self.records.get(id).map(|pin| &pin.config)
    }

    pub(super) fn current(&self, agent: &str) -> Result<Option<(&ID, &RecordRef)>> {
        self.resolve(agent, |_| true)
    }

    pub(super) fn bound(&self, id: &ID, parents: &BTreeSet<ID>) -> Result<&RecordRef> {
        let record = self
            .records
            .get(id)
            .ok_or_else(|| Error::Missing("Agent pin Entry".into()))?;
        match self.resolve(&record.config.database, |id| parents.contains(id))? {
            Some((selected, config)) if selected == id => Ok(config),
            _ => Err(Error::InvalidScope(
                "input did not observe this Agent pin".into(),
            )),
        }
    }

    fn resolve(
        &self,
        agent: &str,
        includes: impl Fn(&ID) -> bool,
    ) -> Result<Option<(&ID, &RecordRef)>> {
        let superseded: BTreeSet<_> = self
            .supersedes
            .iter()
            .filter(|(update, _)| includes(update))
            .map(|(_, prior)| prior)
            .collect();
        let mut active = self.records.iter().filter(|(id, pin)| {
            pin.config.database == agent && includes(id) && !superseded.contains(id)
        });
        let selected = active.next();
        if active.next().is_some() {
            return Err(Error::IdentityConflict(
                "concurrent pins for the same Agent".into(),
            ));
        }
        Ok(selected.map(|(id, pin)| (id, &pin.config)))
    }
}

pub(super) async fn read(
    db: &Database,
    at: &Snapshot,
    txn: &Transaction,
    ancestry: &Ancestry,
    conversation: &ConversationRead,
) -> Result<(Pins, super::Originals)> {
    let mut pins = Pins::default();
    let mut originals = super::Originals::default();
    // Store traversal is parent-first (including concurrent siblings). An
    // ancestor's update is therefore known before resolving any descendant.
    for entry in super::entries(db, STORE, at).await? {
        let delta = originals.delta(&entry, STORE)?;
        let check = async {
            if !conversation.pinned_executions {
                return Err(Error::Unsupported(
                    "Agent pins without descriptor capability".into(),
                ));
            }
            let mut parents = ancestry.prefix(&Snapshot::from(entry.parents()?))?;
            parents.insert(entry.id()); // creation may carry both descriptor and first pin
            conversation.require_descriptor(&parents)?;
            parents.remove(entry.id_ref());
            let [text]: [&String; 1] =
                delta.values().collect::<Vec<_>>().try_into().map_err(|_| {
                    Error::Integrity("Agent pin Entry must write exactly one pin".into())
                })?;
            let pin: Pin = atom::decode_v1(text)?;
            if !pin.config.is_agent_config() || ID::parse(&pin.config.database).is_err() {
                return Err(Error::Integrity(
                    "invalid Agent pin record reference".into(),
                ));
            }
            let previous = pins.resolve(&pin.config.database, |id| parents.contains(id))?;
            match (pin.update, previous) {
                (false, None) => {}
                (true, Some((id, config))) if config != &pin.config => {
                    pins.supersedes.insert(entry.id(), id.clone());
                }
                _ => return Err(Error::Integrity("invalid Agent pin join/update".into())),
            }
            pins.records.insert(entry.id(), pin);
            Ok(())
        }
        .await;
        if let Err(error) = check {
            originals.issues.push(error);
        }
    }
    if conversation.pinned_executions || !pins.records.is_empty() {
        if let Err(error) = originals.check_materialized::<PinStore>(txn, STORE).await {
            originals.issues.push(error);
        }
    } else if txn.get_index().await?.get_entry(STORE).await.is_ok() {
        originals.issues.push(Error::Unsupported(
            "Agent pin store without capability".into(),
        ));
    }
    Ok((pins, originals))
}

/// Fixture writer only. The caller opens at an accepted snapshot, checks a
/// unique previous binding (or absence for join), and commits with live auth.
#[cfg(test)]
pub(in crate::session) async fn stage(
    txn: &Transaction,
    config: RecordRef,
    update: bool,
) -> Result<()> {
    super::staging::atom_in(
        &txn.get_store::<PinStore>(STORE).await?,
        &Pin {
            version: 1,
            update,
            config,
        },
    )
    .await
}
