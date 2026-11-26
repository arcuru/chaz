//! Unit tests for the memory tools module. Extracted from `memory.rs`.

use super::*;
use crate::agent::AgentRegistry;
use crate::agent_db::{AgentDbConfig, AgentMeta, MEMORY_STORE, create_agent_db};
use crate::hosted_index::{DbEntry, HostedIndex};
use crate::session::{Session, SessionRegistry};
use crate::tool::{ScopedTools, ToolContext, ToolProfile, ToolRegistry};
use crate::types::ConversationId;
use eidetica::backend::database::InMemory;
use eidetica::{Instance, NewAccount};
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
    fixture_on(agent_name, Box::new(InMemory::new())).await
}

async fn fixture_on(
    agent_name: &str,
    backend: Box<dyn eidetica::backend::BackendImpl>,
) -> (
    Instance,
    Arc<SessionRegistry>,
    HostedIndex,
    Arc<TokioMutex<Session>>,
) {
    let (instance, user) =
        Instance::create_backend(backend, NewAccount::with_password("test", "test-password"))
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
        agent_identity: None,
        turn_request_id: None,
        tool_call_key: None,
        turn_attempt: None,
        tool_call_site: None,
        observation_yield: None,
        call_depth: 0,
        max_call_depth: 10,
        tools: ScopedTools::new(Arc::new(ToolRegistry::new()), None),
        profile: ToolProfile::default(),
        allow_no_reply: false,
        session,
        grants: crate::grants::Grants::default(),
        session_capabilities: crate::grants::Grants::default(),
        agent_capabilities: crate::grants::Grants::default(),
        agent_grants: std::collections::HashMap::new(),
        host: Arc::new(crate::tool_host::NativeToolHost::new()),
        active_extensions: std::collections::HashSet::new(),
        routine_engine: None,
        observation_deadline: None,
        call_deadline: None,
        call_timeout: None,
        call_cancel: None,
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
async fn recall_with_bank_requires_current_direct_read_despite_reference() {
    use crate::agent_db::BankPermission;
    use crate::embedding::EmbedError;
    use eidetica::auth::crypto::PublicKey;
    use eidetica::auth::types::{AuthKey, Permission};
    use eidetica::entry::ID;

    struct RevokeDuringSearch(Arc<SessionRegistry>, ID, PublicKey);

    impl Embedder for RevokeDuringSearch {
        fn model_id(&self) -> &str {
            "test/revoke"
        }

        fn embed<'a>(
            &'a self,
            _text: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<f32>, EmbedError>> + Send + 'a>> {
            Box::pin(async move {
                self.0
                    .revoke_on_memory_bank(&self.1, &self.2)
                    .await
                    .unwrap();
                Ok(vec![1.0])
            })
        }
    }

    let (_instance, registry, index, session) = fixture("alpha").await;
    let bank_root =
        ID::parse(&provision_bank(&registry, "alpha", "patrick", BankPermission::Write).await)
            .unwrap();
    let bank = registry
        .open_memory_bank(&bank_root, None)
        .await
        .unwrap()
        .unwrap();
    let txn = bank.database().new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_global_auth_key(AuthKey::active(None, Permission::Read))
        .await
        .unwrap();
    txn.commit().await.unwrap();

    let remember = Remember::new(registry.clone(), index.clone(), None);
    let recall = Recall::new(registry.clone(), index.clone(), None);
    let caller = index.find_by_name("alpha").unwrap();
    let mut ctx = make_ctx("alpha", session);
    ctx.agent_identity = Some(caller.clone());
    let args = serde_json::json!({ "query": "boss", "bank": "patrick" });
    assert!(recall.execute(args.clone(), &ctx).await.is_err());
    registry
        .grant_on_memory_bank(&bank_root, &caller.pubkey, "alpha", BankPermission::Write)
        .await
        .unwrap();
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

    // Read alone suffices even though the reference still advertises Write.
    registry
        .grant_on_memory_bank(&bank_root, &caller.pubkey, "alpha", BankPermission::Read)
        .await
        .unwrap();
    let found = recall.execute(args.clone(), &ctx).await.unwrap();
    assert!(
        found.contains("boss"),
        "recall should return entry: {found}"
    );
    let missing_identity = make_ctx("alpha", ctx.session.clone());
    assert!(
        recall
            .execute(args.clone(), &missing_identity)
            .await
            .is_err()
    );
    // The native handle can still read under wildcard Auth. A grant checked
    // only at open would disclose the result after the direct slot is revoked.
    let racing_recall = Recall::new(
        registry.clone(),
        index,
        Some(Arc::new(RevokeDuringSearch(
            registry,
            bank_root,
            caller.pubkey,
        ))),
    );
    let error = racing_recall.execute(args.clone(), &ctx).await.unwrap_err();
    assert!(error.to_string().contains("current Read grant"), "{error}");
    assert!(recall.execute(args, &ctx).await.is_err());

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

    let mut ctx = make_ctx("alpha", session);
    ctx.agent_identity = index.find_by_name("alpha");
    let recall = Recall::new(registry.clone(), index, None);
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
    let mut ctx = make_ctx("alpha", session);
    ctx.agent_identity = index.find_by_name("alpha");
    let id = eidetica::entry::ID::parse(&bank_db_id).unwrap();
    registry
        .grant_on_memory_bank(
            &id,
            &ctx.agent_identity.as_ref().unwrap().pubkey,
            "alpha",
            crate::agent_db::BankPermission::Write,
        )
        .await
        .unwrap();
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
    let out = recall
        .execute(
            serde_json::json!({ "query": "friday", "bank": "shared" }),
            &ctx,
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

// ── entry character cap ───────────────────────────────────────────────

#[test]
fn format_entry_truncates_value_to_cap_with_elision() {
    let entry = MemoryEntry {
        key: "cap".into(),
        value: "x".repeat(500),
        timestamp: Utc::now(),
        tags: vec![],
    };
    let out = format_entry(&entry, Some(200));
    assert!(
        out.contains(&format!("{}…", "x".repeat(200))),
        "expected 200-char value with elision marker, got: {out}"
    );
    assert!(
        !out.contains(&"x".repeat(201)),
        "value not truncated: {out}"
    );
}

#[test]
fn format_entry_leaves_short_and_uncapped_values_untouched() {
    let entry = MemoryEntry {
        key: "short".into(),
        value: "brief note".into(),
        timestamp: Utc::now(),
        tags: vec!["t".into()],
    };
    assert!(format_entry(&entry, Some(200)).contains("brief note"));
    assert!(format_entry(&entry, None).contains("brief note"));
}

#[test]
fn truncation_is_char_boundary_safe() {
    // Each é is two bytes, so a byte-level slice at 100 chars would
    // split a character in half. Truncation must keep whole chars.
    let entry = MemoryEntry {
        key: "mb".into(),
        value: "é".repeat(300),
        timestamp: Utc::now(),
        tags: vec![],
    };
    let out = format_entry(&entry, Some(100));
    assert!(
        out.contains(&format!("{}…", "é".repeat(100))),
        "expected exactly 100 multibyte chars + elision, got: {out}"
    );
    assert!(
        !out.contains(&"é".repeat(101)),
        "chars split mid-character: {out}"
    );
}

#[tokio::test]
async fn search_memory_honors_entry_char_cap() {
    let (_instance, registry, index, session) = fixture("alpha").await;
    let remember = Remember::new(registry.clone(), index.clone(), None);
    let ctx = make_ctx("alpha", session);

    let long_value = "y".repeat(500);
    put(&remember, &ctx, "cap", &long_value, &[]).await;

    let entry = index.find_by_name("alpha").expect("agent registered");
    let agent_db = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .expect("agent db open");
    let out = search_memory(
        agent_db.database(),
        MEMORY_STORE,
        "cap",
        &[],
        5,
        Some(20),
        None,
    )
    .await
    .unwrap();
    assert!(
        out.contains(&format!("{}…", "y".repeat(20))),
        "expected 20-char value with elision, got: {out}"
    );
    assert!(!out.contains(&"y".repeat(21)), "value not truncated: {out}");
}

/// No model or embedding provider. Both reads and the copy enter through the
/// tool boundary; the fixture owner only provisions grants and edits source rows.
#[tokio::test]
async fn copy_session_messages_rechecks_grants_and_refreshes_native_bank() {
    exercise_session_copy(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn copy_session_messages_service_read_only_source() {
    exercise_session_copy(true).await;
}

async fn exercise_session_copy(service: bool) {
    use crate::agent_db::BankPermission;
    use crate::session::{EntryType, SessionEntry};
    use crate::tools::session_memory::{BrowseSessions, ImportSessionMessages};
    use eidetica::auth::types::{AuthKey, Permission};
    use serde_json::json;

    let (_instance, owner, index, here) = fixture("alpha").await;
    let bank_id = provision_bank(&owner, "alpha", "notes", BankPermission::Write).await;
    // Only fixture setup uses the owner. In both modes the tool has Read alone.
    #[cfg(unix)]
    let (registry, server) = if service {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("copy.sock");
        let (shutdown, rx) = tokio::sync::watch::channel(());
        let server = eidetica::service::ServiceServer::bind(_instance, &socket)
            .await
            .unwrap();
        let task = tokio::spawn(server.run(rx));
        let client = Instance::connect(format!("unix://{}", socket.display()))
            .await
            .unwrap();
        let user = client.login_account("test", "test-password").await.unwrap();
        let registry = Arc::new(
            SessionRegistry::new(client, user, Arc::new(AgentRegistry::with_default_agent()))
                .await
                .unwrap(),
        );
        (registry, Some((dir, shutdown, task)))
    } else {
        (owner.clone(), None)
    };
    #[cfg(not(unix))]
    let registry = owner.clone();
    let mut ctx = make_ctx("alpha", here.clone());
    let actor = index.find_by_name("alpha").unwrap();
    let browse = BrowseSessions::new(registry.clone());
    let import = ImportSessionMessages(registry.clone());
    assert!(browse.execute(json!({}), &ctx).await.is_err());
    ctx.agent_identity = Some(actor.clone());
    let here_id = here.lock().await.database().root_id().to_string();
    owner
        .attach_agent_to_session(&here_id, &actor)
        .await
        .unwrap();
    let (source_id, db) = owner.create_session(Some("other")).await.unwrap();
    let page_arg = json!({"session_db_id":source_id.0});
    assert!(browse.execute(page_arg.clone(), &ctx).await.is_err());
    owner
        .attach_agent_to_session(&source_id.0, &actor)
        .await
        .unwrap();
    let source = db.new_transaction().await.unwrap();
    source
        .get_settings()
        .unwrap()
        .set_auth_key(&actor.pubkey, AuthKey::active(None, Permission::Read))
        .await
        .unwrap();
    source.commit().await.unwrap();
    let mut msg = SessionEntry {
        sender: "Alice".into(),
        content: "lighthouse".into(),
        timestamp: Utc::now(),
        entry_type: EntryType::Message,
        metadata: None,
        routing: None,
    };
    let here_msg = here.lock().await.add_entry(msg.clone()).await.unwrap();
    let here_arg = json!({"session_db_id":here_id});
    assert!(
        browse
            .execute(here_arg.clone(), &ctx)
            .await
            .unwrap()
            .contains(here_msg.as_str())
    );
    let mut source_session = Session::new(source_id.clone(), db.clone()).await;
    let mut ids = Vec::new();
    for _ in 0..9 {
        ids.push(
            source_session
                .add_entry(msg.clone())
                .await
                .unwrap()
                .to_string(),
        );
    }
    ids.sort();
    let selected = &ids[8];
    let listing = browse.execute(json!({}), &ctx).await.unwrap();
    assert!(listing.contains(&here_id) && listing.contains(&source_id.0));
    let page: Value =
        serde_json::from_str(&browse.execute(page_arg.clone(), &ctx).await.unwrap()).unwrap();
    assert_eq!(
        page["untrusted_message_excerpts"].as_array().unwrap().len(),
        8
    );
    let next = json!({"session_db_id":source_id.0, "cursor":page["next_cursor"].as_str().unwrap()});
    let last: Value =
        serde_json::from_str(&browse.execute(next.clone(), &ctx).await.unwrap()).unwrap();
    assert_eq!(
        last["untrusted_message_excerpts"][0]["message_id"],
        selected.as_str()
    );
    assert!(last["next_cursor"].is_null());
    // After restart cursors expire, but a selected native row ID is durable.
    assert!(
        BrowseSessions::new(registry.clone())
            .execute(next.clone(), &ctx)
            .await
            .is_err()
    );
    let bank_root = eidetica::entry::ID::parse(&bank_id).unwrap();
    let args = json!({"session_db_id":source_id.0,"bank":"notes","message_ids":[selected]});
    owner
        .grant_on_memory_bank(&bank_root, &actor.pubkey, "alpha", BankPermission::Read)
        .await
        .unwrap();
    assert!(import.execute(args.clone(), &ctx).await.is_err());
    owner
        .grant_on_memory_bank(&bank_root, &actor.pubkey, "alpha", BankPermission::Write)
        .await
        .unwrap();
    let bad_batch =
        json!({"session_db_id": source_id.0, "bank": "notes", "message_ids": [selected, "absent"]});
    assert!(import.execute(bad_batch, &ctx).await.is_err());
    // No prefix of the failed batch was committed.
    for expect in [1, 0] {
        let receipt: Value =
            serde_json::from_str(&import.execute(args.clone(), &ctx).await.unwrap()).unwrap();
        assert_eq!(receipt["changed"], expect);
    }
    msg.content = "observatory".into();
    let edit = db.new_transaction().await.unwrap();
    edit.get_store::<Table<SessionEntry>>("entries")
        .await
        .unwrap()
        .set(selected, msg)
        .await
        .unwrap();
    edit.commit().await.unwrap();
    let receipt: Value =
        serde_json::from_str(&import.execute(args.clone(), &ctx).await.unwrap()).unwrap();
    assert_eq!(receipt["changed"], 1);
    let recall = Recall::new(registry, index, None);
    let recalled = recall
        .execute(json!({"query":"observatory", "bank":"notes"}), &ctx)
        .await
        .unwrap();
    assert!(
        recalled.contains("observatory")
            && recalled.contains("Alice")
            && recalled.contains(&format!("session:{}/message:{selected}", source_id.0)),
        "{recalled}"
    );
    assert!(!recalled.contains("lighthouse"));
    // A format trace forbids falling back to old entries, even when a known
    // message ID and all native grants still exist.
    let mixed = here
        .lock()
        .await
        .database()
        .new_transaction()
        .await
        .unwrap();
    mixed
        .get_store::<eidetica::store::DocStore>("conversation_v1")
        .await
        .unwrap()
        .set_string("unknown", "unsupported")
        .await
        .unwrap();
    mixed.commit().await.unwrap();
    assert!(browse.execute(here_arg, &ctx).await.is_err());
    assert!(
        import
            .execute(
                json!({"session_db_id":here_id, "bank":"notes", "message_ids":[here_msg.as_str()]}),
                &ctx
            )
            .await
            .is_err()
    );
    let revoke = db.new_transaction().await.unwrap();
    revoke
        .get_settings()
        .unwrap()
        .revoke_auth_key(&actor.pubkey)
        .await
        .unwrap();
    revoke.commit().await.unwrap();
    assert!(browse.execute(next, &ctx).await.is_err());
    assert!(import.execute(args, &ctx).await.is_err());
    assert!(
        !browse
            .execute(json!({}), &ctx)
            .await
            .unwrap()
            .contains(&source_id.0)
    );
    #[cfg(unix)]
    if let Some((_dir, shutdown, task)) = server {
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}

/// Opt-in characterization, not a CI timing threshold. The source frontier
/// advances between sizes; this does not simulate an OS cache flush.
#[cfg(unix)]
#[tokio::test]
#[ignore = "SQLite session excerpt workload"]
async fn session_excerpt_growth() {
    use crate::{
        agent_db::BankPermission,
        session::{EntryType, SessionEntry},
        tools::session_memory::{BrowseSessions, ImportSessionMessages},
    };
    use eidetica::{
        auth::types::{AuthKey, Permission},
        backend::database::Sqlite,
    };
    use serde_json::json;
    use std::time::Instant;

    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("corpus.db").display()
    );
    let disk_bytes = || {
        dir.path()
            .read_dir()
            .unwrap()
            .flatten()
            .filter_map(|p| p.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum::<u64>()
    };
    let (instance, owner, index, here) =
        fixture_on("reader", Box::new(Sqlite::connect(&url).await.unwrap())).await;
    let actor = index.find_by_name("reader").unwrap();
    let bank = provision_bank(&owner, "reader", "notes", BankPermission::Write).await;
    owner
        .grant_on_memory_bank(
            &eidetica::entry::ID::parse(&bank).unwrap(),
            &actor.pubkey,
            "reader",
            BankPermission::Write,
        )
        .await
        .unwrap();
    let here_id = here.lock().await.database().root_id().to_string();
    owner
        .attach_agent_to_session(&here_id, &actor)
        .await
        .unwrap();
    let (source, db) = owner.create_session(Some("corpus")).await.unwrap();
    owner
        .attach_agent_to_session(&source.0, &actor)
        .await
        .unwrap();
    let txn = db.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_auth_key(&actor.pubkey, AuthKey::active(None, Permission::Read))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let socket = dir.path().join("service.sock");
    let (shutdown, rx) = tokio::sync::watch::channel(());
    let service = eidetica::service::ServiceServer::bind(instance, &socket)
        .await
        .unwrap();
    let task = tokio::spawn(service.run(rx));
    let client = Instance::connect(format!("unix://{}", socket.display()))
        .await
        .unwrap();
    let user = client.login_account("test", "test-password").await.unwrap();
    let reg = Arc::new(
        SessionRegistry::new(client, user, Arc::new(AgentRegistry::with_default_agent()))
            .await
            .unwrap(),
    );
    let browse = BrowseSessions::new(reg.clone());
    let import = ImportSessionMessages(reg.clone());
    let recall = Recall::new(reg, index, None);
    let mut ctx = make_ctx("reader", here);
    ctx.agent_identity = Some(actor);
    let mut previous = 0;
    for rows in [64, 512, 2048] {
        // A batch of eight records includes four ordinary chat messages plus
        // directive, summary, tool-call and larger tool-result audit records.
        for start in (previous..rows).step_by(8) {
            let txn = db.new_transaction().await.unwrap();
            let entries = txn
                .get_store::<Table<SessionEntry>>("entries")
                .await
                .unwrap();
            for i in start..start + 8 {
                let (kind, bytes) = match i % 8 {
                    4 => (EntryType::Directive, 256),
                    5 => (EntryType::Summary, 512),
                    6 => (EntryType::ToolCall, 256),
                    7 => (EntryType::ToolResult, 16 * 1024),
                    _ => (EntryType::Message, 1024),
                };
                entries
                    .set(
                        format!("row-{i:06}"),
                        SessionEntry {
                            sender: "Alice".into(),
                            content: format!("lighthouse {i} {}", "x".repeat(bytes)),
                            timestamp: Utc::now(),
                            entry_type: kind,
                            metadata: None,
                            routing: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            txn.commit().await.unwrap();
        }
        previous = rows;
        let start_bytes = disk_bytes();
        let args = json!({"session_db_id":source.0});
        let mut pages = vec![];
        let mut page_len = 0;
        let mut next_cursor = Value::Null;
        for _ in 0..4 {
            let t = Instant::now();
            let result = browse.execute(args.clone(), &ctx).await.unwrap();
            pages.push(t.elapsed().as_micros());
            page_len = result.len();
            let page: Value = serde_json::from_str(&result).unwrap();
            assert_eq!(
                page["untrusted_message_excerpts"].as_array().unwrap().len(),
                4
            );
            next_cursor = page["next_cursor"].clone();
        }
        let t = Instant::now();
        let next = browse
            .execute(json!({"session_db_id":source.0,"cursor":next_cursor}), &ctx)
            .await
            .unwrap();
        let next_us = t.elapsed().as_micros();
        assert!(next.contains("row-000008"));
        let id = format!("row-{:06}", rows - 8);
        let copy = json!({"bank":"notes","session_db_id":source.0,"message_ids":[id]});
        let mut imports = vec![];
        for expected in [1, 0] {
            let t = Instant::now();
            let result = import.execute(copy.clone(), &ctx).await.unwrap();
            imports.push(t.elapsed().as_micros());
            let receipt: Value = serde_json::from_str(&result).unwrap();
            assert_eq!(receipt["changed"], expected);
        }
        let t = Instant::now();
        let result = recall
            .execute(json!({"bank":"notes","query":"lighthouse"}), &ctx)
            .await
            .unwrap();
        let recall_us = t.elapsed().as_micros();
        assert!(result.contains(&id));
        eprintln!(
            "rows={rows} page_us={pages:?} next_us={next_us} copy_retry_us={imports:?} recall_us={recall_us} page_bytes={page_len} sqlite_plus_sidecars_before={start_bytes} after={}",
            disk_bytes()
        );
    }
    shutdown.send(()).unwrap();
    task.await.unwrap().unwrap();
}
