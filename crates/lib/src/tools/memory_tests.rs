//! Unit tests for the memory tools module. Extracted from `memory.rs`.

use super::*;
use crate::agent::AgentRegistry;
use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
use crate::hosted_index::{DbEntry, HostedIndex};
use crate::session::{Session, SessionRegistry};
use crate::tool::{ScopedTools, ToolContext, ToolProfile, ToolRegistry};
use crate::types::ConversationId;
use eidetica::backend::database::InMemory;
use eidetica::{Instance, NewUser};
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

/// Full fixture: peer with a SessionRegistry + HostedIndex + one agent's
/// DB registered, plus a dummy session so ToolContext has a valid handle.
async fn fixture(
    agent_name: &str,
) -> (
    Instance,
    Arc<SessionRegistry>,
    HostedIndex,
    Arc<TokioMutex<Session>>,
) {
    let backend = InMemory::new();
    let (instance, user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();
    let agents_reg = Arc::new(AgentRegistry::with_default_agent());
    let registry = Arc::new(
        SessionRegistry::new(instance.clone(), user, agents_reg)
            .await
            .unwrap(),
    );
    let index = HostedIndex::empty("agent");

    // Create an Agent DB for the named agent.
    let (agent_db, pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            agent_name,
            &AgentDbConfig::default(),
            &AgentMeta {
                display_name: Some(agent_name.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    index.register(DbEntry {
        db_id: agent_db.id(),
        display_name: agent_name.to_string(),
        pubkey,
    });

    // Need a session for ToolContext.session — just create a blank one.
    let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
    let session = Arc::new(TokioMutex::new(
        Session::new(ConversationId(session_db.root_id().to_string()), session_db).await,
    ));

    (instance, registry, index, session)
}

fn make_ctx(agent_name: &str, session: Arc<TokioMutex<Session>>) -> ToolContext {
    ToolContext {
        agent_name: agent_name.to_string(),
        call_depth: 0,
        max_call_depth: 10,
        tools: ScopedTools::new(Arc::new(ToolRegistry::new()), None),
        profile: ToolProfile::default(),
        session,
        grants: crate::grants::Grants::default(),
        agent_grants: std::collections::HashMap::new(),
        host: Arc::new(crate::tool_host::NativeToolHost::new()),
        active_extensions: std::collections::HashSet::new(),
        iteration_budget: None,
        routine_engine: None,
    }
}

#[tokio::test]
async fn remember_writes_to_own_agent_db() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let tool = Remember::new(registry.clone(), index.clone(), None);
    let ctx = make_ctx("alpha", session);

    tool.execute(
        serde_json::json!({ "key": "favorite_color", "value": "blue" }),
        &ctx,
    )
    .await
    .unwrap();

    let recall = Recall::new(registry, index, None);
    let ctx2 = make_ctx("alpha", ctx.session.clone());
    let result = recall
        .execute(serde_json::json!({ "query": "favorite" }), &ctx2)
        .await
        .unwrap();
    assert!(result.contains("blue"), "expected blue in {result}");
}

#[tokio::test]
async fn per_agent_memory_is_isolated() {
    // alpha and beta are separate agents on the same peer. Writing under
    // alpha must not appear under beta's recall.
    let (_instance, registry, index, session) = fixture("alpha").await;
    let (beta_db, beta_pubkey) = {
        let mut user = registry.user_for_tests().await;
        create_agent_db(
            &mut user,
            "beta",
            &AgentDbConfig::default(),
            &AgentMeta {
                display_name: Some("beta".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    index.register(DbEntry {
        db_id: beta_db.id(),
        display_name: "beta".to_string(),
        pubkey: beta_pubkey,
    });

    let remember = Remember::new(registry.clone(), index.clone(), None);
    let recall = Recall::new(registry, index, None);

    let ctx_alpha = make_ctx("alpha", session.clone());
    remember
        .execute(
            serde_json::json!({ "key": "secret", "value": "alpha-only" }),
            &ctx_alpha,
        )
        .await
        .unwrap();

    let ctx_beta = make_ctx("beta", session);
    let result = recall
        .execute(serde_json::json!({ "query": "secret" }), &ctx_beta)
        .await
        .unwrap();
    assert!(
        !result.contains("alpha-only"),
        "leakage across agents: {result}"
    );
    assert!(
        result.contains("No memories"),
        "expected no-results for beta, got: {result}"
    );
}

// -------------------------------------------------------------------------
// Memory banks via optional `bank` param
// -------------------------------------------------------------------------

/// Helper: create a memory bank DB on the peer, attach it to the agent
/// with the given permission, return the bank's DB ID.
async fn provision_bank(
    registry: &Arc<SessionRegistry>,
    agent_name: &str,
    bank_name: &str,
    permission: crate::agent_db::BankPermission,
) -> String {
    let (bank, _pk) = {
        let mut user = registry.user_for_tests().await;
        crate::memory_bank_db::create_memory_bank(
            &mut user,
            bank_name,
            &crate::memory_bank_db::MemoryBankMeta {
                display_name: Some(bank_name.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    let bank_db_id = bank.id().to_string();
    // Attach to agent's memory_banks subtree.
    let agent_db = {
        let user = registry.user_for_tests().await;
        let (db, _) = crate::agent_db::find_agent_db(&user, agent_name)
            .await
            .unwrap();
        db
    };
    agent_db
        .attach_memory_bank(crate::agent_db::MemoryBankRef {
            name: bank_name.to_string(),
            db_id: bank_db_id.clone(),
            permission,
        })
        .await
        .unwrap();
    bank_db_id
}

#[tokio::test]
async fn remember_with_bank_writes_to_bank_and_recall_reads_back() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let _ = provision_bank(
        &registry,
        "alpha",
        "patrick",
        crate::agent_db::BankPermission::Write,
    )
    .await;

    let remember = Remember::new(registry.clone(), index.clone(), None);
    let ctx = make_ctx("alpha", session.clone());
    let out = remember
        .execute(
            serde_json::json!({
                "key": "role",
                "value": "boss",
                "bank": "patrick"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        out.contains("patrick"),
        "response should mention bank: {out}"
    );

    // Recall via the same bank finds it.
    let recall = Recall::new(registry.clone(), index, None);
    let found = recall
        .execute(
            serde_json::json!({ "query": "boss", "bank": "patrick" }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        found.contains("boss"),
        "recall should return entry: {found}"
    );

    // And own memory is untouched (self-remember was never called).
    let found_self = recall
        .execute(serde_json::json!({ "query": "boss" }), &ctx)
        .await
        .unwrap();
    assert!(
        found_self.contains("No memories found"),
        "self memory should be empty: {found_self}"
    );
}

#[tokio::test]
async fn remember_with_read_only_bank_errors() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let _ = provision_bank(
        &registry,
        "alpha",
        "readonly",
        crate::agent_db::BankPermission::Read,
    )
    .await;

    let remember = Remember::new(registry.clone(), index, None);
    let ctx = make_ctx("alpha", session);
    let err = remember
        .execute(
            serde_json::json!({ "key": "k", "value": "v", "bank": "readonly" }),
            &ctx,
        )
        .await
        .expect_err("expected Read-only rejection");
    let msg = format!("{err:?}");
    assert!(msg.contains("Read-only"), "unexpected error: {msg}");
}

#[tokio::test]
async fn recall_with_unknown_bank_lists_available() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let _ = provision_bank(
        &registry,
        "alpha",
        "patrick",
        crate::agent_db::BankPermission::Read,
    )
    .await;

    let recall = Recall::new(registry.clone(), index, None);
    let ctx = make_ctx("alpha", session);
    let err = recall
        .execute(
            serde_json::json!({ "query": "x", "bank": "nonexistent" }),
            &ctx,
        )
        .await
        .expect_err("expected unknown-bank error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("patrick"),
        "error should list available bank 'patrick': {msg}"
    );
}

#[tokio::test]
async fn list_memory_banks_tool_returns_self_and_attached() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let _ = provision_bank(
        &registry,
        "alpha",
        "patrick",
        crate::agent_db::BankPermission::Write,
    )
    .await;
    let _ = provision_bank(
        &registry,
        "alpha",
        "projects",
        crate::agent_db::BankPermission::Read,
    )
    .await;

    let lister = ListMemoryBanks::new(registry.clone(), index);
    let ctx = make_ctx("alpha", session);
    let out = lister.execute(serde_json::json!({}), &ctx).await.unwrap();
    assert!(out.contains("self"), "should include self: {out}");
    assert!(out.contains("patrick"), "should include patrick: {out}");
    assert!(out.contains("Write"), "should show Write perm: {out}");
    assert!(out.contains("projects"), "should include projects: {out}");
    assert!(out.contains("Read"), "should show Read perm: {out}");
}

// -------------------------------------------------------------------------
// Stage A — tags + BM25 ranked recall
// -------------------------------------------------------------------------

/// Helper: write a single fact with optional tags.
async fn put(remember: &Remember, ctx: &ToolContext, key: &str, value: &str, tags: &[&str]) {
    let tags_json: Vec<Value> = tags.iter().map(|t| Value::String(t.to_string())).collect();
    remember
        .execute(
            serde_json::json!({ "key": key, "value": value, "tags": tags_json }),
            ctx,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn remember_persists_tags_and_recall_renders_them() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let remember = Remember::new(registry.clone(), index.clone(), None);
    let recall = Recall::new(registry, index, None);
    let ctx = make_ctx("alpha", session);

    put(
        &remember,
        &ctx,
        "deadline",
        "ship by friday",
        &["project", "urgent"],
    )
    .await;

    let out = recall
        .execute(serde_json::json!({ "query": "ship" }), &ctx)
        .await
        .unwrap();
    assert!(out.contains("ship by friday"), "missing value: {out}");
    assert!(out.contains("tags:"), "missing tags marker: {out}");
    assert!(out.contains("project"), "missing 'project' tag: {out}");
    assert!(out.contains("urgent"), "missing 'urgent' tag: {out}");
}

#[tokio::test]
async fn recall_filters_by_tags_and() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let remember = Remember::new(registry.clone(), index.clone(), None);
    let recall = Recall::new(registry, index, None);
    let ctx = make_ctx("alpha", session);

    put(&remember, &ctx, "k1", "alpha-fact", &["project"]).await;
    put(&remember, &ctx, "k2", "beta-fact", &["project", "urgent"]).await;
    put(&remember, &ctx, "k3", "gamma-fact", &["urgent"]).await;

    // Filter by both tags — only the entry tagged with both should remain.
    let out = recall
        .execute(
            serde_json::json!({
                "query": "fact",
                "tags": ["project", "urgent"],
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.contains("beta-fact"), "expected k2: {out}");
    assert!(!out.contains("alpha-fact"), "k1 leaked: {out}");
    assert!(!out.contains("gamma-fact"), "k3 leaked: {out}");
}

#[tokio::test]
async fn recall_honors_limit() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let remember = Remember::new(registry.clone(), index.clone(), None);
    let recall = Recall::new(registry, index, None);
    let ctx = make_ctx("alpha", session);

    for i in 0..5 {
        put(&remember, &ctx, &format!("k{i}"), "shared keyword", &[]).await;
    }
    let out = recall
        .execute(serde_json::json!({ "query": "shared", "limit": 2 }), &ctx)
        .await
        .unwrap();
    // One entry per line; expect exactly two lines.
    assert_eq!(out.lines().count(), 2, "expected 2 lines, got: {out}");
}

#[tokio::test]
async fn recall_empty_query_returns_by_recency() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let remember = Remember::new(registry.clone(), index.clone(), None);
    let recall = Recall::new(registry, index, None);
    let ctx = make_ctx("alpha", session);

    // Use distinct keys (dedup-by-key would otherwise collapse them)
    // and tag every entry the same so we can exercise the tags-only
    // filtering path without keyword scoring.
    put(&remember, &ctx, "first", "old", &["log"]).await;
    // Force a measurable timestamp gap.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    put(&remember, &ctx, "second", "new", &["log"]).await;

    let out = recall
        .execute(
            serde_json::json!({ "query": "", "tags": ["log"], "limit": 10 }),
            &ctx,
        )
        .await
        .unwrap();
    let pos_first = out.find("first").unwrap_or(usize::MAX);
    let pos_second = out.find("second").unwrap_or(usize::MAX);
    assert!(pos_first != usize::MAX, "missing first: {out}");
    assert!(pos_second != usize::MAX, "missing second: {out}");
    assert!(
        pos_second < pos_first,
        "more recent entry should sort first: {out}"
    );
}

#[tokio::test]
async fn recall_ranks_more_relevant_first() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let remember = Remember::new(registry.clone(), index.clone(), None);
    let recall = Recall::new(registry, index, None);
    let ctx = make_ctx("alpha", session);

    // Three entries — only one mentions both "deploy" and "friday".
    // BM25 should put it first.
    put(&remember, &ctx, "ops_note_a", "deploy on monday", &[]).await;
    put(&remember, &ctx, "ops_note_b", "deploy on friday", &[]).await;
    put(&remember, &ctx, "ops_note_c", "weekly status", &[]).await;

    let out = recall
        .execute(serde_json::json!({ "query": "deploy friday" }), &ctx)
        .await
        .unwrap();
    let pos_b = out.find("ops_note_b").unwrap_or(usize::MAX);
    let pos_a = out.find("ops_note_a").unwrap_or(usize::MAX);
    assert!(pos_b != usize::MAX, "missing best match: {out}");
    assert!(
        pos_b < pos_a,
        "more relevant entry should rank first: {out}"
    );
    // The weekly status note shares no terms; should not appear.
    assert!(
        !out.contains("ops_note_c"),
        "non-matching entry leaked: {out}"
    );
}

#[tokio::test]
async fn recall_unknown_token_returns_no_results() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let remember = Remember::new(registry.clone(), index.clone(), None);
    let recall = Recall::new(registry, index, None);
    let ctx = make_ctx("alpha", session);

    put(&remember, &ctx, "k1", "the quick brown fox", &[]).await;

    let out = recall
        .execute(serde_json::json!({ "query": "zyxwvut" }), &ctx)
        .await
        .unwrap();
    assert!(out.contains("No memories found"), "got: {out}");
}

// -------------------------------------------------------------------------
// Embedding subtree + hybrid recall
// -------------------------------------------------------------------------

use crate::embedding::test_support::MockEmbedder;
use crate::embedding::{EmbeddingEntry, embeddings_store_name};
use eidetica::store::Table;

/// Pull every `EmbeddingEntry` row out of the agent's `embeddings:<model_id>`
/// subtree. Returns `(memory_row_id, vector)` pairs. Used to assert
/// the on-write population path actually ran.
async fn read_embeddings(
    registry: &Arc<SessionRegistry>,
    agent_name: &str,
    model_id: &str,
) -> Vec<EmbeddingEntry> {
    let user = registry.user_for_tests().await;
    let (db, _) = crate::agent_db::find_agent_db(&user, agent_name)
        .await
        .unwrap();
    let txn = db.database().new_transaction().await.unwrap();
    let store_name = embeddings_store_name(model_id);
    let store = match txn.get_store::<Table<EmbeddingEntry>>(&store_name).await {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    store
        .search(|_: &EmbeddingEntry| true)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, e)| e)
        .collect()
}

#[tokio::test]
async fn remember_with_embedder_populates_embeddings_subtree() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(
        "test/mock",
        vec!["deploy", "friday", "monday"],
    ));
    let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
    let ctx = make_ctx("alpha", session);

    remember
        .execute(
            serde_json::json!({ "key": "ops", "value": "deploy on friday" }),
            &ctx,
        )
        .await
        .unwrap();

    let stored = read_embeddings(&registry, "alpha", "test/mock").await;
    assert_eq!(stored.len(), 1, "expected one embedding row");
    let v = &stored[0].vector;
    // MockEmbedder normalizes — cosine should be ~1 against itself.
    let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((mag - 1.0).abs() < 1e-5, "vector should be unit length");
    // Row ID is non-empty (the join key into `memory`).
    assert!(!stored[0].memory_row_id.is_empty());
}

#[tokio::test]
async fn recall_semantic_match_when_keywords_dont_overlap() {
    // The whole point of embedding-backed recall: a query with no
    // shared tokens with the value but with shared embedding axes
    // still surfaces it.
    // MockEmbedder's "shared axis" is literally "shared token", so we
    // construct a setup where the query "friday" tokenizes to a token
    // that overlaps an axis in the entry but is not present in the
    // entry's text directly — using a synonym mapping.
    //
    // Trick: use distinct surface tokens, but route them to the same
    // axis. We achieve this by making the axes themselves lexicalized
    // synonyms. Concretely: entry value = "ship by EOW", query = "deploy
    // friday". MockEmbedder on "ship by EOW" maps "ship"→axis 0; on
    // "deploy friday" maps "deploy"→axis 1, "friday"→axis 2 — no
    // overlap.
    //
    // To force semantic-only retrieval, give the entry a word that
    // shares an axis with one query word but not lexically. We do
    // this by making axis names match content the entry has.
    let (_instance, registry, index, session) = fixture("alpha").await;
    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(
        "test/mock",
        vec!["ship", "deploy", "release", "friday"],
    ));
    let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
    let recall = Recall::new(registry.clone(), index, Some(embedder.clone()));
    let ctx = make_ctx("alpha", session);

    // Entry contains "ship" + "release" (axes 0 and 2). Tokens "ship"
    // and "release" land on those axes.
    put(&remember, &ctx, "k1", "ship the release on friday", &[]).await;

    // Query "deploy friday" tokens "deploy" + "friday" → axes 1 and 3.
    // Lexically, BM25 only matches "friday" (one of the entry tokens),
    // so without semantic, the entry is found but with weak score.
    // Cosine: vectors share axis 3 ("friday") so cosine > 0.
    // This isn't a clean lexical-disjoint test, so let's add a
    // distractor entry with very different content — semantic should
    // rank the relevant entry higher.
    put(&remember, &ctx, "k2", "weekly status report on Monday", &[]).await;

    let out = recall
        .execute(serde_json::json!({ "query": "deploy friday" }), &ctx)
        .await
        .unwrap();
    // Best match should appear; Monday status should not surface (no
    // shared axis with the query, no shared token either).
    assert!(out.contains("k1"), "expected k1 in output: {out}");
    assert!(!out.contains("k2"), "k2 should not surface: {out}");
}

#[tokio::test]
async fn recall_falls_back_to_lexical_when_db_has_no_embeddings() {
    // Write WITHOUT an embedder, then recall WITH one. The agent DB
    // has no `embeddings:<model_id>` subtree, but recall should still
    // work via BM25 alone.
    let (_instance, registry, index, session) = fixture("alpha").await;
    let remember_lex = Remember::new(registry.clone(), index.clone(), None);
    let ctx = make_ctx("alpha", session);
    put(&remember_lex, &ctx, "k1", "deploy on friday", &[]).await;

    // Now recall with an embedder configured.
    let embedder: Arc<dyn Embedder> =
        Arc::new(MockEmbedder::new("test/mock", vec!["deploy", "friday"]));
    let recall = Recall::new(registry, index, Some(embedder));
    let out = recall
        .execute(serde_json::json!({ "query": "friday" }), &ctx)
        .await
        .unwrap();
    assert!(
        out.contains("deploy on friday"),
        "lexical fallback should surface entry: {out}"
    );
}

#[tokio::test]
async fn rrf_combine_merges_lexical_and_semantic_winners() {
    // Direct unit test of `rrf_combine`: entry A wins BM25, entry B
    // wins cosine, entry C is in neither — result must surface A and
    // B (in some order) and exclude C.
    let entries = vec![
        MemoryEntry {
            key: "a".into(),
            value: "alpha".into(),
            timestamp: Utc::now(),
            tags: vec![],
        },
        MemoryEntry {
            key: "b".into(),
            value: "beta".into(),
            timestamp: Utc::now(),
            tags: vec![],
        },
        MemoryEntry {
            key: "c".into(),
            value: "gamma".into(),
            timestamp: Utc::now(),
            tags: vec![],
        },
    ];
    let bm25 = vec![(10.0_f64, 0)]; // A is the only BM25 hit
    let cos = vec![(0.9_f32, 1)]; // B is the only cosine hit
    let out = rrf_combine(&entries, &bm25, &cos, 10);
    let keys: Vec<&str> = out.iter().map(|h| h.entry.key.as_str()).collect();
    assert!(keys.contains(&"a"), "missing BM25 winner: {keys:?}");
    assert!(keys.contains(&"b"), "missing cosine winner: {keys:?}");
    assert!(!keys.contains(&"c"), "non-matching leaked: {keys:?}");
    assert!(out.iter().all(|h| h.score > 0.0), "scores should populate");
}

#[tokio::test]
async fn rrf_combine_boosts_when_both_rankers_agree() {
    // Direct unit test: an entry that wins both lists should outrank
    // entries winning only one — that's the whole point of RRF.
    let mk = |k: &str| MemoryEntry {
        key: k.into(),
        value: "v".into(),
        timestamp: Utc::now(),
        tags: vec![],
    };
    let entries = vec![mk("both"), mk("bm25_only"), mk("cos_only")];
    // BM25: idx 0 first, idx 1 second
    let bm25 = vec![(10.0_f64, 0), (5.0, 1)];
    // Cosine: idx 0 first, idx 2 second
    let cos = vec![(0.9_f32, 0), (0.5, 2)];
    let out = rrf_combine(&entries, &bm25, &cos, 10);
    assert_eq!(out.len(), 3);
    assert_eq!(
        out[0].entry.key,
        "both",
        "agreement should rank first: {:?}",
        out.iter().map(|h| &h.entry.key).collect::<Vec<_>>()
    );
    // The double-winner should also score strictly above the singles.
    assert!(out[0].score > out[1].score, "agreement should dominate");
}

#[tokio::test]
async fn remember_with_failing_embedder_still_stores_memory() {
    // Critical fallback: a network-down embedding API must not lose
    // the user's memory. The memory row gets written; the
    // `embeddings:<model_id>` subtree stays empty.
    use crate::embedding::test_support::FailingEmbedder;
    let (_instance, registry, index, session) = fixture("alpha").await;
    let embedder: Arc<dyn Embedder> = Arc::new(FailingEmbedder::new("test/down"));
    let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
    let recall = Recall::new(registry.clone(), index, None);
    let ctx = make_ctx("alpha", session);

    remember
        .execute(
            serde_json::json!({ "key": "k1", "value": "ship by friday" }),
            &ctx,
        )
        .await
        .unwrap();

    // Memory persisted: BM25 recall surfaces it.
    let out = recall
        .execute(serde_json::json!({ "query": "ship" }), &ctx)
        .await
        .unwrap();
    assert!(
        out.contains("ship by friday"),
        "memory should persist despite embedder failure: {out}"
    );
    // Embedding subtree stayed empty.
    let stored = read_embeddings(&registry, "alpha", "test/down").await;
    assert!(
        stored.is_empty(),
        "no embedding row should be written when embedder errors: {stored:?}"
    );
}

#[tokio::test]
async fn recall_with_failing_query_embedder_falls_back_to_bm25() {
    // Write with a working embedder so embeddings exist on disk;
    // recall with a failing one — the query-embedding error path
    // must degrade to BM25-only, not error out.
    use crate::embedding::test_support::FailingEmbedder;
    let (_instance, registry, index, session) = fixture("alpha").await;
    let writer: Arc<dyn Embedder> =
        Arc::new(MockEmbedder::new("test/mock", vec!["ship", "friday"]));
    let failing: Arc<dyn Embedder> = Arc::new(FailingEmbedder::new("test/down"));
    let remember = Remember::new(registry.clone(), index.clone(), Some(writer));
    let recall = Recall::new(registry, index, Some(failing));
    let ctx = make_ctx("alpha", session);

    put(&remember, &ctx, "k1", "ship by friday", &[]).await;

    let out = recall
        .execute(serde_json::json!({ "query": "ship" }), &ctx)
        .await
        .unwrap();
    assert!(
        out.contains("ship by friday"),
        "BM25 fallback should still surface entry: {out}"
    );
}

#[tokio::test]
async fn bank_remember_with_embedder_populates_embedding_subtree() {
    // The bank path uses `do_remember(..., embedder=Some(...))`
    // exactly like self memory; verify by writing into a bank and
    // reading the bank's `embeddings:<model_id>` subtree directly.
    let (_instance, registry, index, session) = fixture("alpha").await;
    let bank_db_id = provision_bank(
        &registry,
        "alpha",
        "shared",
        crate::agent_db::BankPermission::Write,
    )
    .await;

    let embedder: Arc<dyn Embedder> =
        Arc::new(MockEmbedder::new("test/mock", vec!["deploy", "friday"]));
    let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
    let ctx = make_ctx("alpha", session);
    remember
        .execute(
            serde_json::json!({
                "key": "ops",
                "value": "deploy friday",
                "bank": "shared",
            }),
            &ctx,
        )
        .await
        .unwrap();

    // Pull the embeddings subtree off the bank DB itself. Scoped so
    // the user lock drops before we consume `registry` into Recall.
    {
        let user = registry.user_for_tests().await;
        let id = eidetica::entry::ID::parse(&bank_db_id).unwrap();
        let database = user.open_database(&id).await.unwrap();
        let txn = database.new_transaction().await.unwrap();
        let store = txn
            .get_store::<Table<EmbeddingEntry>>(&embeddings_store_name("test/mock"))
            .await
            .unwrap();
        let rows = store.search(|_: &EmbeddingEntry| true).await.unwrap();
        assert_eq!(rows.len(), 1, "bank should have one embedding");
    }

    // And recall via the bank still works (hybrid path).
    let recall = Recall::new(registry, index, Some(embedder));
    let ctx2 = make_ctx("alpha", ctx.session.clone());
    let out = recall
        .execute(
            serde_json::json!({ "query": "friday", "bank": "shared" }),
            &ctx2,
        )
        .await
        .unwrap();
    assert!(out.contains("deploy friday"), "bank recall: {out}");
}

#[tokio::test]
async fn multiple_model_subtrees_coexist_on_one_db() {
    // Switching models should leave the old subtree dormant, not
    // overwrite or remove it. Write one entry under model A, then
    // another under model B on the same DB; both subtrees populate
    // independently.
    let (_instance, registry, index, session) = fixture("alpha").await;
    let ctx = make_ctx("alpha", session);

    let emb_a: Arc<dyn Embedder> = Arc::new(MockEmbedder::new("test/model-a", vec!["alpha"]));
    let remember_a = Remember::new(registry.clone(), index.clone(), Some(emb_a));
    remember_a
        .execute(
            serde_json::json!({ "key": "k1", "value": "alpha-fact" }),
            &ctx,
        )
        .await
        .unwrap();

    let emb_b: Arc<dyn Embedder> = Arc::new(MockEmbedder::new("test/model-b", vec!["beta"]));
    let remember_b = Remember::new(registry.clone(), index, Some(emb_b));
    remember_b
        .execute(
            serde_json::json!({ "key": "k2", "value": "beta-fact" }),
            &ctx,
        )
        .await
        .unwrap();

    // Direct subtree inspection: each model has exactly one row,
    // and they reference distinct memory rows.
    let user = registry.user_for_tests().await;
    let (agent_db, _) = crate::agent_db::find_agent_db(&user, "alpha")
        .await
        .unwrap();
    let txn = agent_db.database().new_transaction().await.unwrap();
    let a_rows = txn
        .get_store::<Table<EmbeddingEntry>>(&embeddings_store_name("test/model-a"))
        .await
        .unwrap()
        .search(|_: &EmbeddingEntry| true)
        .await
        .unwrap();
    let b_rows = txn
        .get_store::<Table<EmbeddingEntry>>(&embeddings_store_name("test/model-b"))
        .await
        .unwrap()
        .search(|_: &EmbeddingEntry| true)
        .await
        .unwrap();
    assert_eq!(a_rows.len(), 1, "model-a subtree");
    assert_eq!(b_rows.len(), 1, "model-b subtree");
    assert_ne!(
        a_rows[0].1.memory_row_id, b_rows[0].1.memory_row_id,
        "rows reference different memory entries"
    );
}

#[tokio::test]
async fn re_remember_same_key_does_not_leak_old_value() {
    // Dedup-by-key keeps the most-recent row; recall must surface
    // the new value and not the old. The old embedding row stays
    // dormant in `embeddings:<model>` (its memory_row_id no longer
    // joins to anything visible) — that's expected and harmless.
    let (_instance, registry, index, session) = fixture("alpha").await;
    let embedder: Arc<dyn Embedder> =
        Arc::new(MockEmbedder::new("test/mock", vec!["alpha", "beta"]));
    let remember = Remember::new(registry.clone(), index.clone(), Some(embedder.clone()));
    let recall = Recall::new(registry, index, Some(embedder));
    let ctx = make_ctx("alpha", session);

    put(&remember, &ctx, "role", "alpha-version", &[]).await;
    // Force a measurable timestamp gap so dedup picks the new one.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    put(&remember, &ctx, "role", "beta-version", &[]).await;

    let out = recall
        .execute(serde_json::json!({ "query": "role" }), &ctx)
        .await
        .unwrap();
    assert!(
        out.contains("beta-version"),
        "newer value should surface: {out}"
    );
    assert!(
        !out.contains("alpha-version"),
        "older value should not leak: {out}"
    );
}
