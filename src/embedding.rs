//! Embedder abstraction — Searchable Memory Stage 2.
//!
//! Chaz writes embeddings into per-DB subtrees named `embeddings:<model_id>`,
//! where `<model_id>` is the canonical token returned by [`Embedder::model_id`]
//! (e.g. `openai/text-embedding-3-small`). Multiple model subtrees can
//! coexist on the same DB — the runtime always reads/writes the one
//! matching the configured embedder.
//!
//! The trait is intentionally minimal: one async `embed(text)` call
//! returning an L2-normalized vector. Callers do cosine similarity
//! against stored vectors; with normalized vectors that reduces to a
//! plain dot product.
//!
//! Production impl is [`OpenAiEmbedder`] — calls any OpenAI-compatible
//! `/v1/embeddings` endpoint. A test-only [`MockEmbedder`] lives in
//! `cfg(test)` so unit tests don't need an API key.

use crate::security::SecretStore;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

/// Errors from an embedding call. Network/transient errors are retryable
/// at the caller; configuration errors are not.
#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("Embedding API error: {0}")]
    Api(String),
    #[error("Embedding configuration error: {0}")]
    Configuration(String),
    #[error("Embedding network error: {0}")]
    Network(String),
}

/// Convert an embedder vector to the canonical store entry. The
/// embedder is responsible for L2 normalization; we just stash the
/// floats alongside the memory row ID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmbeddingEntry {
    /// Row ID returned by `Table::insert(memory_entry)` — joins back to
    /// the `memory` subtree on the same DB.
    pub memory_row_id: String,
    /// Unit-length embedding vector. Cosine similarity to a query
    /// vector reduces to dot product.
    pub vector: Vec<f32>,
}

/// Standard subtree-name format: `embeddings:<model_id>`. Multiple
/// model variants coexist on a single DB by living under different
/// subtrees, all syncing alongside the parent `memory` subtree.
pub fn embeddings_store_name(model_id: &str) -> String {
    format!("embeddings:{model_id}")
}

/// Embedder trait. Object-safe via `Pin<Box<Future>>` (same pattern as
/// `Tool::execute`).
pub trait Embedder: Send + Sync {
    /// Stable identifier: `<provider>/<model>`. Used to name the
    /// `embeddings:<model_id>` subtree on each memory DB.
    fn model_id(&self) -> &str;

    /// Embed a single text and return a unit-length vector.
    fn embed<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<f32>, EmbedError>> + Send + 'a>>;
}

/// Cosine similarity between two equal-length vectors. With pre-normalized
/// vectors this equals their dot product; the L2 fallback handles any
/// drift.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = (na.sqrt() * nb.sqrt()).max(f32::EPSILON);
    dot / denom
}

/// In-place L2 normalize. No-op for the zero vector.
pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// =====================================================================
// OpenAI-compatible HTTP embedder
// =====================================================================

/// Static config for [`OpenAiEmbedder`]. Mirrors [`crate::config::Backend`]
/// in spirit but without the chat-only fields (models list, retries, etc).
#[derive(Debug, Clone)]
pub struct OpenAiEmbedderConfig {
    /// e.g. `https://api.openai.com/v1`
    pub api_base: String,
    /// Model name as the API expects it: `text-embedding-3-small`.
    pub model: String,
    /// Provider tag used to namespace the model id — defaults to
    /// `openai`. Override when pointing at an OpenAI-compatible third
    /// party so the stored subtree name distinguishes them.
    pub provider: String,
    /// Reference key into `SecretStore`.
    pub api_key_ref: String,
}

/// OpenAI-compatible `/v1/embeddings` client. Pulls the API key from
/// the `SecretStore` per request — same pattern as the chat backend.
pub struct OpenAiEmbedder {
    config: OpenAiEmbedderConfig,
    secrets: SecretStore,
    model_id: String,
    client: reqwest::Client,
}

impl OpenAiEmbedder {
    pub fn new(config: OpenAiEmbedderConfig, secrets: SecretStore) -> Result<Self, EmbedError> {
        let model_id = format!("{}/{}", config.provider, config.model);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| EmbedError::Configuration(format!("HTTP client init failed: {e}")))?;
        Ok(Self {
            config,
            secrets,
            model_id,
            client,
        })
    }
}

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingsDatum>,
}

#[derive(Deserialize)]
struct EmbeddingsDatum {
    embedding: Vec<f32>,
}

impl Embedder for OpenAiEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn embed<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<f32>, EmbedError>> + Send + 'a>> {
        Box::pin(async move {
            let api_key = self.secrets.get(&self.config.api_key_ref).ok_or_else(|| {
                EmbedError::Configuration(format!(
                    "API key missing for embedder ref '{}'",
                    self.config.api_key_ref
                ))
            })?;
            let url = format!("{}/embeddings", self.config.api_base.trim_end_matches('/'));
            let req = EmbeddingsRequest {
                model: &self.config.model,
                input: text,
            };
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&api_key)
                .json(&req)
                .send()
                .await
                .map_err(|e| EmbedError::Network(format!("embeddings request failed: {e}")))?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(EmbedError::Api(format!("embeddings HTTP {status}: {body}")));
            }
            let body: EmbeddingsResponse = resp
                .json()
                .await
                .map_err(|e| EmbedError::Api(format!("embeddings decode failed: {e}")))?;
            let mut vector = body
                .data
                .into_iter()
                .next()
                .ok_or_else(|| EmbedError::Api("empty data array".into()))?
                .embedding;
            l2_normalize(&mut vector);
            Ok(vector)
        })
    }
}

/// Construct the configured `Arc<dyn Embedder>` from chaz's [`crate::config::EmbeddingConfig`],
/// returning `None` when no embedding backend is configured. Errors are
/// fatal — they indicate a malformed config rather than a transient
/// runtime issue.
pub fn build_embedder(
    cfg: Option<&crate::config::EmbeddingConfig>,
    secrets: &SecretStore,
) -> Result<Option<Arc<dyn Embedder>>, EmbedError> {
    let Some(cfg) = cfg else { return Ok(None) };
    match cfg.backend {
        crate::config::EmbeddingBackend::OpenAI => {
            let api_base = cfg
                .api_base
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let api_key_ref = cfg.api_key_ref.clone().ok_or_else(|| {
                EmbedError::Configuration(
                    "embedding.api_key_ref missing — set api_key in config".into(),
                )
            })?;
            let provider = cfg.provider.clone().unwrap_or_else(|| "openai".to_string());
            let inner = OpenAiEmbedder::new(
                OpenAiEmbedderConfig {
                    api_base,
                    model: cfg.model.clone(),
                    provider,
                    api_key_ref,
                },
                secrets.clone(),
            )?;
            Ok(Some(Arc::new(inner)))
        }
    }
}

#[cfg(test)]
pub mod test_support {
    //! Test-only embedder helpers. `MockEmbedder` returns a deterministic
    //! token-bag vector so tests can control which strings are "near";
    //! `FailingEmbedder` always errors so tests can exercise the
    //! lexical-fallback path.
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Embedder that always returns a configured `EmbedError`. Used to
    /// verify that write/recall paths degrade gracefully when the
    /// embedding service is down.
    pub struct FailingEmbedder {
        pub model_id: String,
    }

    impl FailingEmbedder {
        pub fn new(model_id: impl Into<String>) -> Self {
            Self {
                model_id: model_id.into(),
            }
        }
    }

    impl Embedder for FailingEmbedder {
        fn model_id(&self) -> &str {
            &self.model_id
        }
        fn embed<'a>(
            &'a self,
            _text: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<f32>, EmbedError>> + Send + 'a>> {
            Box::pin(async move { Err(EmbedError::Network("simulated failure".into())) })
        }
    }

    /// Hand-tuned embedder: every test text gets a fixed length-N vector
    /// computed from a `HashMap<token, axis>` lookup. Two texts share an
    /// axis when they share the underlying token; cosine similarity then
    /// reduces to "fraction of shared tokens."
    pub struct MockEmbedder {
        pub model_id: String,
        pub axes: Vec<String>,
        pub call_count: Mutex<usize>,
    }

    impl MockEmbedder {
        pub fn new(model_id: impl Into<String>, axes: Vec<&str>) -> Self {
            Self {
                model_id: model_id.into(),
                axes: axes.into_iter().map(String::from).collect(),
                call_count: Mutex::new(0),
            }
        }

        #[allow(dead_code)]
        pub fn calls(&self) -> usize {
            *self.call_count.lock().unwrap()
        }
    }

    impl Embedder for MockEmbedder {
        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn embed<'a>(
            &'a self,
            text: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<f32>, EmbedError>> + Send + 'a>> {
            Box::pin(async move {
                *self.call_count.lock().unwrap() += 1;
                let lower = text.to_lowercase();
                let mut counts: HashMap<&str, f32> = HashMap::new();
                for token in lower.split(|c: char| !c.is_alphanumeric()) {
                    if !token.is_empty() {
                        *counts.entry(token).or_insert(0.0) += 1.0;
                    }
                }
                let mut v: Vec<f32> = self
                    .axes
                    .iter()
                    .map(|axis| *counts.get(axis.as_str()).unwrap_or(&0.0))
                    .collect();
                l2_normalize(&mut v);
                Ok(v)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_name_format() {
        assert_eq!(
            embeddings_store_name("openai/text-embedding-3-small"),
            "embeddings:openai/text-embedding-3-small"
        );
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!((cosine_similarity(&a, &b)).abs() < 1e-6);
    }

    #[test]
    fn cosine_identical_is_one() {
        let a = vec![0.6, 0.8];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_unequal_or_empty() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
    }

    #[test]
    fn l2_normalize_unit_length() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_zero_is_noop() {
        let mut v = vec![0.0, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.0]);
    }

    #[tokio::test]
    async fn mock_embedder_shares_axes_for_shared_tokens() {
        use test_support::MockEmbedder;
        let e = MockEmbedder::new("test/mock", vec!["deploy", "friday", "monday", "weekly"]);
        let v_b = e.embed("deploy on friday").await.unwrap();
        let v_a = e.embed("deploy on monday").await.unwrap();
        let v_c = e.embed("weekly status").await.unwrap();
        // Same shared token "deploy" → nonzero similarity between a & b.
        assert!(cosine_similarity(&v_b, &v_a) > 0.0);
        // No shared tokens → zero similarity.
        assert!(cosine_similarity(&v_b, &v_c) < 1e-6);
    }

    /// Tiny one-shot HTTP/1.1 server: accepts one connection, captures
    /// the raw request, replies with a canned 200 + body, then exits.
    /// Returns the bound address and a join handle yielding the captured
    /// request bytes. Lets us exercise `OpenAiEmbedder` against a real
    /// socket without dragging in `wiremock`.
    async fn one_shot_http_server(
        body: &'static str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let captured = String::from_utf8_lossy(&buf[..n]).to_string();
            // Drain any continuation if Content-Length-bounded body
            // wasn't fully read in the first chunk; not strictly needed
            // for this small test but keeps things robust.
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
            captured
        });
        (addr, handle)
    }

    /// Build a SecretStore over an isolated in-memory eidetica DB —
    /// same pattern as `backends::tests::empty_secrets`.
    async fn empty_secret_store() -> SecretStore {
        use eidetica::backend::database::InMemory;
        use eidetica::Instance;
        let instance = Instance::open(Box::new(InMemory::new())).await.unwrap();
        let _ = instance.create_user("t", None).await;
        let mut user = instance.login_user("t", None).await.unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = eidetica::crdt::Doc::new();
        s.set("name", "central");
        let db = user.create_database(s, &key).await.unwrap();
        SecretStore::new(db).await
    }

    #[tokio::test]
    async fn openai_embedder_calls_v1_embeddings_with_bearer() {
        // Stuff the API key in the SecretStore so the embedder can pull
        // it out at request time, mirroring production wiring.
        let secrets = empty_secret_store().await;
        secrets
            .insert("embedding:openai/test-model".into(), "sk-canary".into())
            .await;

        let canned = r#"{"data":[{"embedding":[3.0,4.0]}]}"#;
        let (addr, server_handle) = one_shot_http_server(canned).await;

        let cfg = OpenAiEmbedderConfig {
            api_base: format!("http://{addr}/v1"),
            model: "test-model".into(),
            provider: "openai".into(),
            api_key_ref: "embedding:openai/test-model".into(),
        };
        let embedder = OpenAiEmbedder::new(cfg, secrets).unwrap();
        let v = embedder.embed("hello world").await.unwrap();

        // Vector came back unit-normalized: |(3,4)| = 5 → (0.6, 0.8).
        assert!((v[0] - 0.6).abs() < 1e-6, "got {v:?}");
        assert!((v[1] - 0.8).abs() < 1e-6, "got {v:?}");

        // Inspect the raw request the server received.
        let captured = server_handle.await.unwrap();
        assert!(
            captured.starts_with("POST /v1/embeddings"),
            "wrong path: {}",
            captured.lines().next().unwrap_or("")
        );
        assert!(
            captured
                .to_lowercase()
                .contains("authorization: bearer sk-canary"),
            "missing bearer header"
        );
        assert!(
            captured.contains(r#""model":"test-model""#),
            "missing model in body: {captured}"
        );
        assert!(
            captured.contains(r#""input":"hello world""#),
            "missing input in body: {captured}"
        );
    }

    #[tokio::test]
    async fn openai_embedder_propagates_http_error() {
        let secrets = empty_secret_store().await;
        secrets
            .insert("embedding:openai/m".into(), "sk-x".into())
            .await;
        // Server replies with a 500.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await.unwrap();
            let body = "boom";
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
        });

        let cfg = OpenAiEmbedderConfig {
            api_base: format!("http://{addr}/v1"),
            model: "m".into(),
            provider: "openai".into(),
            api_key_ref: "embedding:openai/m".into(),
        };
        let embedder = OpenAiEmbedder::new(cfg, secrets).unwrap();
        let err = embedder.embed("x").await.expect_err("expected API error");
        let _ = handle.await;
        assert!(matches!(err, EmbedError::Api(_)), "got: {err:?}");
    }
}
