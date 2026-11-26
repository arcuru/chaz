//! Publication order is an index in the selected ledger, not execution state.
//! The head, index, fire and occurrence commit together. Old unindexed history
//! remains inspectable and inert; losing the head is never permission to reset it.
use super::*;
use eidetica::{Snapshot, store::TableCursor};

pub(super) const HEAD: &str = "schedule_handoff_head";
pub(super) const HANDOFFS: &str = "schedule_handoff_order";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    version: u32,
    owner: String,
    last: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    version: u32,
    sequence: u64,
    reference: ScheduleReference,
}

fn key(sequence: u64) -> String {
    format!("{sequence:020}")
}

async fn head(txn: &eidetica::Transaction, agent: &AgentDb) -> anyhow::Result<Option<Head>> {
    let head = optional(&txn.get_store::<Table<Head>>(HEAD).await?, "head").await?;
    if let Some(head) = &head {
        anyhow::ensure!(
            head.version == 1 && head.owner == agent.id().to_string() && head.last > 0,
            "schedule publication head differs or has an unsupported version"
        );
    }
    Ok(head)
}

pub(super) async fn append(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    reference: &ScheduleReference,
    unpublished_only: bool,
) -> anyhow::Result<u64> {
    let publications = txn.get_store::<Table<Publication>>(HANDOFFS).await?;
    let last = match head(txn, agent).await? {
        Some(head) => head.last,
        None => {
            // The completed snapshot audit has examined ALL occurrence rows,
            // including other lifetimes. This is not absence inferred from a
            // point lookup or a new process, and does not replay legacy fires.
            anyhow::ensure!(unpublished_only, "schedule publication head missing; held");
            anyhow::ensure!(
                publications.scan_page(None, 1).await?.rows.is_empty(),
                "schedule publication index exists without its head"
            );
            0
        }
    };
    let sequence = last
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("schedule publication overflow"))?;
    anyhow::ensure!(
        optional(&publications, &key(sequence)).await?.is_none(),
        "schedule publication sequence already exists; held"
    );
    publications
        .set(
            &key(sequence),
            Publication {
                version: 1,
                sequence,
                reference: reference.clone(),
            },
        )
        .await?;
    txn.get_store::<Table<Head>>(HEAD)
        .await?
        .set(
            "head",
            Head {
                version: 1,
                owner: agent.id().to_string(),
                last: sequence,
            },
        )
        .await?;
    Ok(sequence)
}

pub(super) async fn validate_link(
    txn: &eidetica::Transaction,
    agent: &AgentDb,
    row: &Occurrence,
) -> anyhow::Result<()> {
    let Some(sequence) = row.publication_sequence else {
        return Ok(());
    };
    let head = head(txn, agent)
        .await?
        .ok_or_else(|| anyhow::anyhow!("schedule publication head missing"))?;
    let publication = txn
        .get_store::<Table<Publication>>(HANDOFFS)
        .await?
        .get(&key(sequence))
        .await?;
    anyhow::ensure!(
        sequence <= head.last
            && publication.version == 1
            && publication.sequence == sequence
            && publication.reference == row.reference,
        "schedule publication index differs"
    );
    Ok(())
}

/// One bounded immutable scan in committed publication order, independent of
/// wall-clock timestamps and hash/UUID ordering. Its cursor grants no authority.
pub struct HandoffScan {
    owner: eidetica::entry::ID,
    ledger: eidetica::entry::ID,
    snapshot: Snapshot,
    cursor: Option<TableCursor>,
    last: u64,
    observed: u64,
    finished: bool,
}

impl HandoffScan {
    pub async fn open(agent: &AgentDb) -> anyhow::Result<Option<Self>> {
        let Some(db) = agent.scheduling_database().await? else {
            return Ok(None);
        };
        let snapshot = db.snapshot().await?;
        let txn = db.new_transaction_at(&snapshot).await?;
        let Some(head) = head(&txn, agent).await? else {
            anyhow::ensure!(
                txn.get_store::<Table<Publication>>(HANDOFFS)
                    .await?
                    .scan_page(None, 1)
                    .await?
                    .rows
                    .is_empty(),
                "schedule publication index exists without its head"
            );
            return Ok(None); // unindexed legacy history never becomes new work
        };
        Ok(Some(Self {
            owner: agent.id(),
            ledger: db.root_id().clone(),
            snapshot,
            cursor: None,
            last: head.last,
            observed: 0,
            finished: false,
        }))
    }

    pub async fn next_page(
        &mut self,
        agent: &AgentDb,
        limit: usize,
    ) -> anyhow::Result<Option<Vec<Occurrence>>> {
        anyhow::ensure!((1..=128).contains(&limit), "invalid handoff page size");
        let db = agent
            .scheduling_database()
            .await?
            .ok_or_else(|| anyhow::anyhow!("schedule ledger unavailable"))?;
        anyhow::ensure!(
            agent.id() == self.owner && db.root_id() == &self.ledger,
            "schedule handoff scan identity differs"
        );
        if self.finished {
            return Ok(None);
        }
        let txn = db.new_transaction_at(&self.snapshot).await?;
        let page = txn
            .get_store::<Table<Publication>>(HANDOFFS)
            .await?
            .scan_page(self.cursor.as_ref(), limit)
            .await?;
        let mut observed = self.observed;
        let mut rows = Vec::with_capacity(page.rows.len());
        for (stored_key, publication) in page.rows {
            observed = observed
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("schedule publication overflow"))?;
            anyhow::ensure!(
                publication.version == 1
                    && publication.sequence == observed
                    && stored_key == key(observed),
                "schedule publication order is incomplete or conflicting"
            );
            let row = retry::read_delivery(
                &txn,
                agent,
                &publication.reference.occurrence_id,
                &publication.reference.delivery_id,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("published schedule occurrence missing"))?;
            anyhow::ensure!(
                row.reference == publication.reference
                    && row.publication_sequence == Some(observed),
                "published schedule occurrence differs"
            );
            rows.push(row);
        }
        anyhow::ensure!(
            observed <= self.last && (page.next.is_some() || observed == self.last),
            "schedule publication head/index differ"
        );
        self.observed = observed;
        self.finished = page.next.is_none();
        self.cursor = page.next;
        Ok(Some(rows))
    }
}
