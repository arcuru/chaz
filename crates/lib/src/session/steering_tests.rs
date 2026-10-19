use super::*;
use crate::test_support::counting_backend::{EntryReads, counting_eidetica};
use eidetica::{Instance, Snapshot, entry::ID};

const FIRST: &str = "ffffffff-ffff-4fff-8fff-ffffffffffff";
const SECOND: &str = "00000000-0000-4000-8000-000000000000";

async fn session_db() -> (Instance, Database, EntryReads) {
    let (instance, mut user, reads) = counting_eidetica().await;
    let key = user.get_default_key().unwrap();
    let mut doc = eidetica::crdt::Doc::new();
    doc.set("name", "steering-reads");
    let db = user.create_database(doc, &key).await.unwrap();
    (instance, db, reads)
}

/// Commits that never touch the input store, standing in for transcript and
/// lifecycle history.
async fn unrelated(db: &Database, count: usize) {
    for n in 0..count {
        let txn = db.new_transaction().await.unwrap();
        txn.get_store::<Table<String>>("entries")
            .await
            .unwrap()
            .set(format!("row-{n}"), format!("message {n}"))
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
}

fn input(id: &str, text: &str) -> JobInputRequest {
    JobInputRequest {
        id: id.into(),
        attempt_id: "attempt".into(),
        text: text.into(),
    }
}

/// One input per commit, as `submit_turn_input` publishes, optionally on a
/// branch from an older snapshot.
async fn publish(db: &Database, at: Option<&Snapshot>, request: &JobInputRequest) -> ID {
    let txn = match at {
        Some(snapshot) => db.new_transaction_at(snapshot).await.unwrap(),
        None => db.new_transaction().await.unwrap(),
    };
    txn.get_store::<Table<JobInputRequest>>(INPUTS)
        .await
        .unwrap()
        .set(&request.id, request.clone())
        .await
        .unwrap();
    txn.commit().await.unwrap()
}

fn ids(inputs: &[JobInputRequest]) -> Vec<&str> {
    inputs.iter().map(|r| r.id.as_str()).collect()
}

async fn ordered_at(db: &Database, tips: Vec<ID>) -> Vec<JobInputRequest> {
    ordered_inputs_at(db, &Snapshot::from(tips)).await.unwrap()
}

/// Raw Entry reads for one steering render of a session with `history`
/// unrelated commits and no input publication.
async fn absent_store_reads(history: usize) -> (usize, usize) {
    let (_instance, db, reads) = session_db().await;
    unrelated(&db, history).await;
    let snapshot = db.snapshot().await.unwrap();

    let before = reads.count();
    assert!(ordered_inputs_at(&db, &snapshot).await.unwrap().is_empty());
    let ordering = reads.count() - before;

    let session = Session::new(
        crate::types::ConversationId(db.root_id().to_string()),
        db.clone(),
    )
    .await;
    let before = reads.count();
    assert!(session.turn_inputs_at(&snapshot).await.unwrap().is_empty());
    (ordering, reads.count() - before)
}

#[tokio::test]
async fn absent_input_store_never_reads_main_history() {
    let (short_order, short_render) = absent_store_reads(4).await;
    let (long_order, long_render) = absent_store_reads(64).await;
    // Each raw Entry read is one service GetEntry round trip. A render must
    // not walk the main DAG looking for a store that was never written.
    assert_eq!(
        (short_order, short_render),
        (long_order, long_render),
        "steering reads grew with unrelated history"
    );
    assert!(
        long_order < 4,
        "ordering an empty input store read {long_order} Entries"
    );
}

#[tokio::test]
async fn inputs_keep_causal_order_across_unrelated_commits_and_pinned_snapshots() {
    let (_instance, db, _) = session_db().await;
    unrelated(&db, 3).await;
    let before_inputs = db.snapshot().await.unwrap().into_tips();
    // Row IDs sort opposite to publication order; causal order must win.
    publish(&db, None, &input(FIRST, "first")).await;
    unrelated(&db, 5).await;
    let after_first = db.snapshot().await.unwrap().into_tips();
    publish(&db, None, &input(SECOND, "second")).await;
    unrelated(&db, 5).await;

    let current = ordered_inputs_at(&db, &db.snapshot().await.unwrap())
        .await
        .unwrap();
    assert_eq!(ids(&current), [FIRST, SECOND]);
    assert_eq!(current[1].text, "second");
    // Immutable snapshot boundaries: later publications stay invisible.
    assert_eq!(ids(&ordered_at(&db, after_first).await), [FIRST]);
    assert!(ordered_at(&db, before_inputs).await.is_empty());
}

#[tokio::test]
async fn concurrent_inputs_use_native_canonical_order() {
    let (_instance, db, _) = session_db().await;
    unrelated(&db, 2).await;
    let base = db.snapshot().await.unwrap();
    let first = publish(&db, Some(&base), &input(FIRST, "left")).await;
    let second = publish(&db, Some(&base), &input(SECOND, "right")).await;
    unrelated(&db, 2).await;

    // Expected order comes from Eidetica's canonical store ordering of the
    // two incomparable publications, not from row keys or publisher clocks.
    let canonical: Vec<ID> = db
        .get_store_entries(
            INPUTS,
            &[first.clone(), second.clone()],
            eidetica::service::protocol::ReadScope::Verified,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.id())
        .collect();
    let expected: Vec<&str> = canonical
        .iter()
        .map(|id| if *id == first { FIRST } else { SECOND })
        .collect();

    let snapshot = db.snapshot().await.unwrap();
    let ordered = ordered_inputs_at(&db, &snapshot).await.unwrap();
    assert_eq!(ids(&ordered), expected);
    assert_eq!(
        ids(&ordered_inputs_at(&db, &snapshot).await.unwrap()),
        expected,
        "deterministic"
    );
    // Each branch alone sees only its own publication.
    assert_eq!(ids(&ordered_at(&db, vec![first]).await), [FIRST]);
    assert_eq!(ids(&ordered_at(&db, vec![second]).await), [SECOND]);
}
