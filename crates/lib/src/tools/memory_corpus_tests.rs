//! Corpus-scale exercises for the memory search pipeline.
//!
//! The unit tests in `memory_tests.rs` pin behaviour on hand-built
//! three-to-five-entry sets. This module runs the same pipeline over a
//! 200-entry corpus generated from the project's own documentation
//! (`testdata/memory_corpus.json`, built by
//! `dev/memory-corpus/build_corpus.py`) and measures recall rather than
//! asserting on a single expected string.
//!
//! Two query sets ship with the corpus:
//!
//! - **Title queries** — one per entry, the section heading on its own.
//!   Known-item retrieval where the query shares the document's
//!   vocabulary. This is the lexical ranker's best case.
//! - **Paraphrase queries** — 28 hand-written questions that
//!   deliberately avoid the document's wording. This is where a lexical
//!   ranker is expected to struggle.
//!
//! Everything here runs offline. The semantic leg uses a deterministic
//! bag-of-axes [`MockEmbedder`], so the hybrid numbers below are a
//! *mechanism* check (does fusion fire, does the join hold at scale, do
//! per-model subtrees stay separate) and not a quality claim about any
//! real embedding model. Measured quality against a live embedding
//! endpoint is recorded in `docs/src/user_guide/memory.md`; the
//! `live_*` test at the bottom of this file is the harness that
//! produced it and is `#[ignore]`d because it needs a server.

use super::*;
use crate::agent_db::{MEMORY_STORE, MemoryEntry};
use crate::embedding::test_support::MockEmbedder;
use crate::memory_bank_db::{MemoryBankMeta, create_memory_bank};
use eidetica::backend::database::InMemory;
use eidetica::{Instance, NewUser};
use serde::Deserialize;

const CORPUS_JSON: &str = include_str!("testdata/memory_corpus.json");

#[derive(Debug, Deserialize)]
struct Corpus {
    entries: Vec<CorpusEntry>,
    paraphrase_queries: Vec<ParaphraseQuery>,
}

#[derive(Debug, Deserialize)]
struct CorpusEntry {
    key: String,
    value: String,
    tags: Vec<String>,
    title_query: String,
}

#[derive(Debug, Deserialize)]
struct ParaphraseQuery {
    query: String,
    gold: String,
}

fn corpus() -> Corpus {
    serde_json::from_str(CORPUS_JSON).expect("corpus fixture parses")
}

/// A standalone memory DB holding the whole corpus. Uses a memory-bank
/// DB because it is the smallest thing in the codebase that owns a
/// `memory` Table — no session, agent registry or tool context needed.
async fn load_corpus(
    entries: &[CorpusEntry],
    embedder: Option<&dyn Embedder>,
) -> (Instance, eidetica::Database) {
    let (instance, mut user) =
        Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("corpus"))
            .await
            .unwrap();
    let (bank, _pk) = create_memory_bank(&mut user, "corpus", &MemoryBankMeta::default())
        .await
        .unwrap();
    let db = bank.database().clone();
    for (i, e) in entries.iter().enumerate() {
        write_memory_entry(
            &db,
            MEMORY_STORE,
            MemoryEntry {
                key: e.key.clone(),
                value: e.value.clone(),
                tags: e.tags.clone(),
                // Distinct, increasing timestamps: the dedupe-by-key
                // step compares them, and identical stamps would make
                // the recency path non-deterministic.
                timestamp: chrono::Utc::now() + chrono::Duration::milliseconds(i as i64),
            },
            embedder,
        )
        .await
        .unwrap();
    }
    (instance, db)
}

/// Axes for the offline semantic leg: every token that appears in a
/// paraphrase query. A bag-of-axes embedder only "knows" the axes it is
/// given, so this is the largest vocabulary the mock can usefully carry.
fn paraphrase_axes(c: &Corpus) -> Vec<String> {
    let mut axes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for q in &c.paraphrase_queries {
        for t in tokenize(&q.query) {
            axes.insert(t);
        }
    }
    axes.into_iter().collect()
}

async fn rank_of_gold(
    db: &eidetica::Database,
    query: &str,
    gold: &str,
    k: usize,
    embedder: Option<&dyn Embedder>,
) -> Option<usize> {
    let hits = search_memory_structured(db, MEMORY_STORE, query, &[], k, embedder)
        .await
        .unwrap();
    hits.iter().position(|h| h.entry.key == gold)
}

/// Fraction of queries whose gold entry lands in the top `k`.
async fn recall_at_k(
    db: &eidetica::Database,
    queries: &[(String, String)],
    k: usize,
    embedder: Option<&dyn Embedder>,
) -> f64 {
    let mut hit = 0usize;
    for (q, gold) in queries {
        if rank_of_gold(db, q, gold, k, embedder).await.is_some() {
            hit += 1;
        }
    }
    hit as f64 / queries.len() as f64
}

fn title_queries(c: &Corpus) -> Vec<(String, String)> {
    c.entries
        .iter()
        .map(|e| (e.title_query.clone(), e.key.clone()))
        .collect()
}

fn paraphrase_pairs(c: &Corpus) -> Vec<(String, String)> {
    c.paraphrase_queries
        .iter()
        .map(|q| (q.query.clone(), q.gold.clone()))
        .collect()
}

// -------------------------------------------------------------------------
// Corpus shape
// -------------------------------------------------------------------------

#[test]
fn corpus_is_non_trivial_and_well_formed() {
    let c = corpus();
    assert_eq!(c.entries.len(), 200, "corpus size changed; regenerate docs");
    let keys: std::collections::HashSet<&str> = c.entries.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(keys.len(), c.entries.len(), "duplicate keys in corpus");
    for q in &c.paraphrase_queries {
        assert!(
            keys.contains(q.gold.as_str()),
            "paraphrase gold missing from corpus: {}",
            q.gold
        );
    }
}

// -------------------------------------------------------------------------
// Lexical baseline at corpus scale
// -------------------------------------------------------------------------

/// BM25 alone on known-item queries. The floor is deliberately well
/// under the measured value — this guards against a ranking regression,
/// not against normal drift as the docs change.
#[tokio::test]
async fn bm25_title_recall_at_corpus_scale() {
    let c = corpus();
    let (_i, db) = load_corpus(&c.entries, None).await;
    let queries = title_queries(&c);
    let r1 = recall_at_k(&db, &queries, 1, None).await;
    let r5 = recall_at_k(&db, &queries, 5, None).await;
    println!(
        "bm25 title recall@1={r1:.3} recall@5={r5:.3} (n={})",
        queries.len()
    );
    assert!(r1 >= 0.60, "BM25 recall@1 regressed: {r1:.3}");
    assert!(r5 >= 0.80, "BM25 recall@5 regressed: {r5:.3}");
    assert!(r5 >= r1, "recall@5 must dominate recall@1");
}

/// The paraphrase set is the lexical ranker's weak case, and pinning
/// how weak is the point: it is the baseline any semantic leg has to
/// beat. The upper bound catches a fixture that has drifted into
/// sharing the corpus's vocabulary and stopped being a paraphrase set.
#[tokio::test]
async fn bm25_paraphrase_recall_is_the_baseline_to_beat() {
    let c = corpus();
    let (_i, db) = load_corpus(&c.entries, None).await;
    let queries = paraphrase_pairs(&c);
    let r5 = recall_at_k(&db, &queries, 5, None).await;
    println!("bm25 paraphrase recall@5={r5:.3} (n={})", queries.len());
    assert!(
        r5 < 0.90,
        "paraphrase queries have drifted toward the corpus wording ({r5:.3})"
    );
}

/// A query that shares no token with the corpus must return nothing at
/// all rather than an arbitrary tail — RRF with one empty ranker used to
/// be the place where a "match everything weakly" bug would hide.
#[tokio::test]
async fn nonsense_query_returns_nothing_at_corpus_scale() {
    let c = corpus();
    let (_i, db) = load_corpus(&c.entries, None).await;
    let hits = search_memory_structured(&db, MEMORY_STORE, "zzqqxv wobblefrump", &[], 10, None)
        .await
        .unwrap();
    assert!(hits.is_empty(), "got {} spurious hits", hits.len());
}

// -------------------------------------------------------------------------
// Tag filtering at corpus scale
// -------------------------------------------------------------------------

/// Tag filtering is a hard pre-filter: nothing outside the tag may
/// surface, however well it scores lexically.
#[tokio::test]
async fn tag_filter_is_a_hard_prefilter() {
    let c = corpus();
    let (_i, db) = load_corpus(&c.entries, None).await;
    let filter = vec!["design".to_string()];
    let hits = search_memory_structured(&db, MEMORY_STORE, "session", &filter, 50, None)
        .await
        .unwrap();
    assert!(!hits.is_empty(), "tag filter dropped everything");
    for h in &hits {
        assert!(
            h.entry.tags.iter().any(|t| t == "design"),
            "tag filter leaked {}",
            h.entry.key
        );
    }
    // And it is genuinely narrowing: the same query unfiltered reaches
    // entries the filter excludes.
    let unfiltered = search_memory_structured(&db, MEMORY_STORE, "session", &[], 50, None)
        .await
        .unwrap();
    assert!(
        unfiltered
            .iter()
            .any(|h| !h.entry.tags.iter().any(|t| t == "design")),
        "unfiltered search returned only design entries; filter proves nothing"
    );
}

/// Two tags AND together, and the result is exactly the intersection —
/// checked against the fixture rather than against the search's own
/// output.
#[tokio::test]
async fn multi_tag_filter_ands_at_corpus_scale() {
    let c = corpus();
    let expected: Vec<&str> = c
        .entries
        .iter()
        .filter(|e| {
            e.tags.iter().any(|t| t == "user_guide") && e.tags.iter().any(|t| t == "memory")
        })
        .map(|e| e.key.as_str())
        .collect();
    assert!(
        !expected.is_empty(),
        "fixture has no user_guide+memory entries to test with"
    );
    let (_i, db) = load_corpus(&c.entries, None).await;
    let filter = vec!["user_guide".to_string(), "memory".to_string()];
    let hits = search_memory_structured(&db, MEMORY_STORE, "", &filter, 200, None)
        .await
        .unwrap();
    let got: std::collections::HashSet<&str> = hits.iter().map(|h| h.entry.key.as_str()).collect();
    let want: std::collections::HashSet<&str> = expected.into_iter().collect();
    assert_eq!(got, want, "AND filter did not return the intersection");
}

/// Tag matching is case-insensitive, and stays so with 200 entries in
/// front of it.
#[tokio::test]
async fn tag_filter_is_case_insensitive_at_corpus_scale() {
    let c = corpus();
    let (_i, db) = load_corpus(&c.entries, None).await;
    let lower = search_memory_structured(
        &db,
        MEMORY_STORE,
        "",
        &["user_guide".to_string()],
        200,
        None,
    )
    .await
    .unwrap();
    let upper = search_memory_structured(
        &db,
        MEMORY_STORE,
        "",
        &["USER_GUIDE".to_string()],
        200,
        None,
    )
    .await
    .unwrap();
    assert!(!lower.is_empty());
    assert_eq!(lower.len(), upper.len());
}

/// A tag nobody carries yields an empty result rather than falling back
/// to unfiltered recency.
#[tokio::test]
async fn unknown_tag_returns_empty_not_everything() {
    let c = corpus();
    let (_i, db) = load_corpus(&c.entries, None).await;
    let hits = search_memory_structured(
        &db,
        MEMORY_STORE,
        "session",
        &["no-such-tag".to_string()],
        50,
        None,
    )
    .await
    .unwrap();
    assert!(hits.is_empty(), "unknown tag returned {} hits", hits.len());
}

// -------------------------------------------------------------------------
// Hybrid mechanism at corpus scale
// -------------------------------------------------------------------------

/// Characterizes the cost of fusing in a *weak* second ranker. RRF
/// weights both legs equally, so a semantic leg that is close to noise
/// does not merely fail to help — it displaces lexical winners out of
/// the top-k. The mock embedder here carries a narrow vocabulary and is
/// close to that worst case, which makes the size of the drop the thing
/// worth pinning: fusion degrades gracefully rather than collapsing.
///
/// This is a property of unweighted RRF, not a defect in the join, and
/// it is the reason `docs/src/user_guide/memory.md` tells operators to
/// measure a candidate embedding model rather than assume it helps.
#[tokio::test]
async fn weak_semantic_leg_degrades_gracefully() {
    let c = corpus();
    let axis_words = paraphrase_axes(&c);
    let axes: Vec<&str> = axis_words.iter().map(String::as_str).collect();
    let embedder = MockEmbedder::new("test/corpus-mock", axes);
    let (_i, db) = load_corpus(&c.entries, Some(&embedder)).await;
    let queries = title_queries(&c);
    let lexical = recall_at_k(&db, &queries, 5, None).await;
    let hybrid = recall_at_k(&db, &queries, 5, Some(&embedder)).await;
    println!("title recall@5 lexical={lexical:.3} hybrid(mock)={hybrid:.3}");
    assert!(
        hybrid >= 0.75,
        "a near-noise semantic leg should cost recall, not destroy it: \
         {lexical:.3} -> {hybrid:.3}"
    );
    assert!(
        hybrid <= lexical,
        "the mock embedder is not supposed to be informative here; \
         {lexical:.3} -> {hybrid:.3} means the axis vocabulary now \
         tracks the corpus and the test has stopped measuring the \
         worst case"
    );
}

/// The row-ID join between `memory` and `embeddings:<model>` has to hold
/// for every row at corpus scale — a partial join would silently shrink
/// the semantic leg and be invisible in a five-entry test.
#[tokio::test]
async fn every_corpus_row_gets_an_embedding() {
    use crate::embedding::{EmbeddingEntry, embeddings_store_name};
    use eidetica::store::Table;

    let c = corpus();
    let embedder = MockEmbedder::new("test/corpus-mock", vec!["session", "agent", "memory"]);
    let (_i, db) = load_corpus(&c.entries, Some(&embedder)).await;

    let txn = db.new_transaction().await.unwrap();
    let mem = txn
        .get_store::<Table<MemoryEntry>>(MEMORY_STORE)
        .await
        .unwrap();
    let mem_rows: std::collections::HashSet<String> = mem
        .search(|_: &MemoryEntry| true)
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let emb = txn
        .get_store::<Table<EmbeddingEntry>>(&embeddings_store_name("test/corpus-mock"))
        .await
        .unwrap();
    let emb_rows: std::collections::HashSet<String> = emb
        .search(|_: &EmbeddingEntry| true)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, e)| e.memory_row_id)
        .collect();

    assert_eq!(mem_rows.len(), c.entries.len());
    assert_eq!(
        emb_rows, mem_rows,
        "embedding subtree does not cover every memory row"
    );
}

/// Two models on one DB: each writes its own subtree, and recall under
/// model B must not read model A's vectors. At corpus scale the failure
/// mode this catches is a cross-model join that "works" because the row
/// IDs happen to overlap.
#[tokio::test]
async fn per_model_subtrees_stay_separate_at_corpus_scale() {
    use crate::embedding::{EmbeddingEntry, embeddings_store_name};
    use eidetica::store::Table;

    let c = corpus();
    let subset: Vec<CorpusEntry> = c.entries.into_iter().take(60).collect();
    let model_a = MockEmbedder::new("test/model-a", vec!["session", "agent"]);
    let (_i, db) = load_corpus(&subset, Some(&model_a)).await;

    let txn = db.new_transaction().await.unwrap();
    assert_eq!(
        txn.get_store::<Table<EmbeddingEntry>>(&embeddings_store_name("test/model-a"))
            .await
            .unwrap()
            .search(|_: &EmbeddingEntry| true)
            .await
            .unwrap()
            .len(),
        subset.len()
    );
    drop(txn);

    // Recall under a model that never wrote anything here: the subtree
    // is absent, so the semantic leg contributes nothing and the search
    // degrades to BM25 rather than erroring or reading model A's rows.
    let model_b = MockEmbedder::new("test/model-b", vec!["session", "agent"]);
    let lexical = search_memory_structured(&db, MEMORY_STORE, "session registry", &[], 5, None)
        .await
        .unwrap();
    let under_b = search_memory_structured(
        &db,
        MEMORY_STORE,
        "session registry",
        &[],
        5,
        Some(&model_b),
    )
    .await
    .unwrap();
    let lex_keys: Vec<&str> = lexical.iter().map(|h| h.entry.key.as_str()).collect();
    let b_keys: Vec<&str> = under_b.iter().map(|h| h.entry.key.as_str()).collect();
    assert_eq!(
        lex_keys, b_keys,
        "unknown model's recall diverged from the lexical baseline"
    );
}

/// Rewriting the same key 200 times leaves one live entry, and it is the
/// newest. Dedupe runs over the whole store on every search, so this is
/// also the guard against an O(n) dedupe that keeps the wrong row.
#[tokio::test]
async fn repeated_keys_dedupe_to_the_newest() {
    let c = corpus();
    let base = &c.entries[0];
    let mut versions: Vec<CorpusEntry> = Vec::new();
    for i in 0..200 {
        versions.push(CorpusEntry {
            key: base.key.clone(),
            value: format!("revision {i} of the same note about sessions"),
            tags: base.tags.clone(),
            title_query: base.title_query.clone(),
        });
    }
    let (_i, db) = load_corpus(&versions, None).await;
    let hits = search_memory_structured(&db, MEMORY_STORE, "revision", &[], 10, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "dedupe left {} live rows", hits.len());
    assert!(
        hits[0].entry.value.contains("revision 199"),
        "dedupe kept the wrong revision: {}",
        hits[0].entry.value
    );
}

// -------------------------------------------------------------------------
// Live embedding endpoint
// -------------------------------------------------------------------------

/// BM25-only vs hybrid recall against a real embedding endpoint.
/// Ignored by default — it needs a reachable OpenAI-compatible
/// `/v1/embeddings` server and makes ~430 calls. Run it with:
///
/// ```text
/// CHAZ_EMBED_API_BASE=http://127.0.0.1:8091/v1 \
/// CHAZ_EMBED_MODEL=embeddinggemma:300m \
/// CHAZ_EMBED_PROVIDER=llama-swap \
/// cargo test -p chaz --all-features live_hybrid_vs_bm25 -- --ignored --nocapture
/// ```
///
/// It asserts nothing about quality — the numbers depend entirely on
/// which model is behind the endpoint. It prints a table; the run that
/// produced the figures in `docs/src/user_guide/memory.md` names its
/// model there.
#[tokio::test]
#[ignore = "needs a live embedding endpoint"]
async fn live_hybrid_vs_bm25() {
    use crate::embedding::{OpenAiEmbedder, OpenAiEmbedderConfig};
    use crate::security::SecretStore;

    let api_base = std::env::var("CHAZ_EMBED_API_BASE").expect("CHAZ_EMBED_API_BASE");
    let model = std::env::var("CHAZ_EMBED_MODEL").expect("CHAZ_EMBED_MODEL");
    let provider = std::env::var("CHAZ_EMBED_PROVIDER").unwrap_or_else(|_| "openai".to_string());

    let secrets = {
        let (_instance, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("secrets"))
                .await
                .unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = eidetica::crdt::Doc::new();
        s.set("name", "central");
        let db = user.create_database(s, &key).await.unwrap();
        SecretStore::new(db).await
    };
    secrets
        .insert("embedding:live".into(), "unused".into())
        .await;
    let embedder = OpenAiEmbedder::new(
        OpenAiEmbedderConfig {
            api_base,
            model: model.clone(),
            provider: provider.clone(),
            api_key_ref: "embedding:live".to_string(),
        },
        secrets,
    )
    .unwrap();

    let c = corpus();
    let (_i, db) = load_corpus(&c.entries, Some(&embedder)).await;

    println!(
        "\nmodel: {provider}/{model}   corpus: {} entries",
        c.entries.len()
    );
    println!(
        "{:<12} {:<8} {:>10} {:>10}",
        "query set", "k", "bm25", "hybrid"
    );
    for (name, queries) in [
        ("title", title_queries(&c)),
        ("paraphrase", paraphrase_pairs(&c)),
    ] {
        for k in [1usize, 3, 5, 10] {
            let lexical = recall_at_k(&db, &queries, k, None).await;
            let hybrid = recall_at_k(&db, &queries, k, Some(&embedder)).await;
            println!("{name:<12} {k:<8} {lexical:>10.3} {hybrid:>10.3}");
        }
    }
}
